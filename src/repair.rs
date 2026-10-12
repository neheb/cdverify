// SPDX-License-Identifier: GPL-3.0-or-later
// Based on CUETools' CTDB verification and repair (C) 2008-2025 Gregory S. Chudov

//! Repairing rips with CTDB parity data.
//!
//! 1. Decode the album once, computing CTDB parity (see [`parity`]) and the
//!    CRC32 of every prefix near the edges of CTDB's CRC window.
//! 2. For each CTDB entry for the disc: find our offset from its syndrome.
//!    An exact match whose CRC32 agrees means the rip is already correct.
//! 3. Otherwise download the entry's parity file, work out the corrections
//!    and accept them only if the corrected CRC32 equals the entry's.
//! 4. Write corrected copies of the affected tracks to the output folder.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

use crate::db::{self, CtdbEntry, Db};
use crate::decode;
use crate::disc::SECTOR_FRAMES;
use crate::flacout;
use crate::parity::{self, Fix, Geometry, MAX_NPAR, ParityEncoder};
use crate::scan::AlbumJob;

/// What one CTDB entry says about the rip.
#[derive(Debug, Clone, Serialize)]
pub enum EntryStatus {
    /// The rip already matches this entry.
    Verified {
        confidence: u32,
        offset: i64,
    },
    /// The rip can be corrected to match this entry.
    Repairable {
        confidence: u32,
        offset: i64,
        samples: usize,
        tracks: Vec<u32>,
    },
    NotRepairable {
        confidence: u32,
        reason: String,
    },
}

/// The overall result of checking a rip against CTDB's parity data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum Outcome {
    /// Matches a CTDB entry exactly.
    Verified,
    Repairable,
    /// A repaired copy was written.
    Repaired,
    NotRepairable,
    Failed,
}

impl Outcome {
    /// Short note for an album's verdict.
    pub fn verdict_note(self) -> &'static str {
        match self {
            Outcome::Verified => "but matches CTDB exactly",
            Outcome::Repairable => "repairable with CTDB",
            Outcome::Repaired => "repaired copy written",
            Outcome::NotRepairable => "not repairable with CTDB",
            Outcome::Failed => "CTDB check failed",
        }
    }
}

pub struct Analysis {
    pub statuses: Vec<EntryStatus>,
    /// The most confident repair, if any.
    plan: Option<(u32, i64, Vec<Fix>)>,
    bounds: Arc<[u64]>,
}

impl Analysis {
    pub fn summary(&self) -> String {
        if let Some(EntryStatus::Verified { confidence, offset }) = self
            .statuses
            .iter()
            .find(|s| matches!(s, EntryStatus::Verified { .. }))
        {
            return format!(
                "Matches CTDB exactly (confidence {confidence}, offset {offset:+}): the rip is \
                 correct for that version of the disc, so there's nothing to repair"
            );
        }
        match self
            .statuses
            .iter()
            .find(|s| matches!(s, EntryStatus::Repairable { .. }))
        {
            Some(EntryStatus::Repairable {
                confidence,
                samples,
                tracks,
                ..
            }) => format!(
                "Repairable: {samples} samples differ in track{} {} (CTDB confidence {confidence})",
                if tracks.len() == 1 { "" } else { "s" },
                tracks
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            _ => {
                let reasons: Vec<String> = self
                    .statuses
                    .iter()
                    .filter_map(|s| match s {
                        EntryStatus::NotRepairable { confidence, reason } => {
                            Some(format!("{reason} (confidence {confidence})"))
                        }
                        _ => None,
                    })
                    .collect();
                match reasons.is_empty() {
                    true => "Not repairable: no CTDB entries for this disc".into(),
                    false => format!("Not repairable: {}", reasons.join("; ")),
                }
            }
        }
    }

    /// Confidence and offset of the CTDB entry the rip matches exactly.
    pub fn verified(&self) -> Option<(u32, i64)> {
        self.statuses.iter().find_map(|s| match s {
            EntryStatus::Verified { confidence, offset } => Some((*confidence, *offset)),
            _ => None,
        })
    }

    pub fn repairable(&self) -> bool {
        self.plan.is_some()
    }

    pub fn outcome(&self) -> Outcome {
        if self
            .statuses
            .iter()
            .any(|s| matches!(s, EntryStatus::Verified { .. }))
        {
            Outcome::Verified
        } else if self.repairable() {
            Outcome::Repairable
        } else {
            Outcome::NotRepairable
        }
    }
}

/// CRC32 (IEEE) of every frame prefix `[0, p)` for `p` in `[start, start + len)`.
struct PrefixCrcs {
    start: u64,
    crcs: Vec<u32>,
}

/// Decodes the album and checks it against CTDB's entries for `toc`.
pub fn analyze(
    job: &AlbumJob,
    toc: &str,
    db: &Db,
    progress: &mut dyn FnMut(f32),
) -> Result<Analysis> {
    let bounds = track_bounds(job)?;
    let samples = *bounds.last().unwrap();
    let entries = db::ctdb_exact(db, toc).context("CTDB lookup")?;
    let stride_frames = entries.iter().find_map(|e| e.stride).unwrap_or(5880);
    let geo = Geometry::new(samples, stride_frames * 2);

    // Pass 1: parity, plus prefix CRCs around both ends of the CRC window for
    // every offset the syndrome search can find (|offset| < stride / 2).
    let reach = stride_frames as u64;
    let (a0, b0) = parity::crc_window(geo, 0);
    let mut windows = [
        PrefixCrcs {
            start: a0.saturating_sub(reach),
            crcs: Vec::new(),
        },
        PrefixCrcs {
            start: b0.saturating_sub(reach),
            crcs: Vec::new(),
        },
    ];
    let mut enc = ParityEncoder::new(geo);
    let mut crc = Crc32::new();
    let mut pos = 0u64;
    for (k, t) in job.tracks.iter().enumerate() {
        let decoded = decode::decode(&t.path, false, |frames| {
            enc.push(frames);
            for &f in frames {
                for w in windows.iter_mut() {
                    if pos >= w.start && pos < w.start + 2 * reach + 1 {
                        w.crcs.push(crc.value());
                    }
                }
                crc.update(&f.to_le_bytes());
                pos += 1;
            }
            progress(pos as f32 / samples as f32);
        })?;
        if decoded.frames != bounds[k + 1] - bounds[k] {
            bail!(
                "track {} decoded to {} frames, expected {}",
                k + 1,
                decoded.frames,
                bounds[k + 1] - bounds[k]
            );
        }
    }
    for w in windows.iter_mut() {
        if pos >= w.start && pos < w.start + 2 * reach + 1 {
            w.crcs.push(crc.value());
        }
    }
    let prefix = |p: u64| -> Option<u32> {
        windows.iter().find_map(|w| {
            p.checked_sub(w.start)
                .and_then(|i| w.crcs.get(i as usize).copied())
        })
    };
    // CRC32 of our rip over CTDB's window at `offset`, after `fixes`.
    let window_crc = |offset: i64, fixes: &[Fix]| -> Option<u32> {
        let (a, b) = parity::crc_window(geo, offset);
        let mut c = prefix(b)? ^ crc32_shift(prefix(a)?, (b - a) * 4);
        for f in fixes {
            let end = (f.word as u64 + 1) * 2;
            let raw = crc32_raw(0, &f.diff.to_le_bytes());
            c ^= crc32_shift(raw, b * 4 - end);
        }
        Some(c)
    };

    let mut statuses = Vec::new();
    let mut plan: Option<(u32, i64, Vec<Fix>)> = None;
    for e in &entries {
        let conf = e.confidence;
        let not = |reason: &str| EntryStatus::NotRepairable {
            confidence: conf,
            reason: reason.into(),
        };
        if e.stride != Some(stride_frames) {
            statuses.push(not("different parity stride"));
            continue;
        }
        let Some(head) = entry_syndrome(e) else {
            statuses.push(not("no syndrome"));
            continue;
        };
        let Some((offset, exact)) = parity::find_offset(&enc, &head) else {
            statuses.push(not("too different from this rip"));
            continue;
        };
        if exact && window_crc(offset, &[]) == e.crc32 {
            statuses.push(EntryStatus::Verified {
                confidence: conf,
                offset,
            });
            continue;
        }
        let Some(url) = &e.parity_url else {
            statuses.push(not("differs, and CTDB has no repair data for it"));
            continue;
        };
        let npar = e.npar.unwrap_or(MAX_NPAR).min(MAX_NPAR);
        let bytes = db::fetch(db, url).context("downloading CTDB parity")?;
        let Some(full) = parity::bytes_to_syndrome(geo.stride, npar, &bytes) else {
            statuses.push(not("parity file too short"));
            continue;
        };
        if full[..npar] != head[..] {
            statuses.push(not("parity file doesn't match its entry"));
            continue;
        }
        match parity::compute_fixes(&enc, &full, npar, offset) {
            None => statuses.push(not("too much damage for the repair data")),
            Some(fixes) if window_crc(offset, &fixes) != e.crc32 => {
                statuses.push(not("corrected audio fails CTDB's CRC check"))
            }
            Some(fixes) => {
                let tracks = affected_tracks(&bounds, &fixes);
                let samples = fixes.len();
                statuses.push(EntryStatus::Repairable {
                    confidence: conf,
                    offset,
                    samples,
                    tracks,
                });
                if plan.as_ref().is_none_or(|p| conf > p.0) {
                    plan = Some((conf, offset, fixes));
                }
            }
        }
    }
    // A rip that exactly matches one entry is that version of the disc;
    // "repairing" it toward another entry would rewrite correct audio.
    if statuses
        .iter()
        .any(|s| matches!(s, EntryStatus::Verified { .. }))
    {
        plan = None;
    }
    Ok(Analysis {
        statuses,
        plan,
        bounds,
    })
}

/// The entry's first-column syndrome (`npar` words).
fn entry_syndrome(e: &CtdbEntry) -> Option<Vec<u16>> {
    let npar = e.npar.unwrap_or(8).min(MAX_NPAR);
    match (&e.syndrome, &e.parity) {
        (Some(s), _) => parity::bytes_to_syndrome(1, npar, &db::base64_decode(s)?),
        (None, Some(p)) => parity::parity_bytes_to_syndrome(8, &db::base64_decode(p)?),
        _ => None,
    }
}

fn track_bounds(job: &AlbumJob) -> Result<Arc<[u64]>> {
    let mut b = vec![0u64];
    for t in &job.tracks {
        let frames = t
            .frames
            .ok_or_else(|| anyhow!("unknown length: {}", t.path.display()))?;
        if frames % SECTOR_FRAMES != 0 {
            bail!("not whole CD sectors: {}", t.path.display());
        }
        b.push(b.last().unwrap() + frames);
    }
    Ok(b.into())
}

fn affected_tracks(bounds: &[u64], fixes: &[Fix]) -> Vec<u32> {
    let mut tracks: Vec<u32> = fixes
        .iter()
        .map(|f| bounds.partition_point(|&b| b <= (f.word / 2) as u64) as u32)
        .collect();
    tracks.dedup();
    tracks
}

/// Checks the album against CTDB's parity and, if it's repairable and `dest`
/// is given, writes corrected copies of the affected tracks there. Returns
/// the outcome and a one-line description.
pub fn run(
    job: &AlbumJob,
    toc: &str,
    db: &Db,
    dest: Option<&Path>,
    progress: &mut dyn FnMut(f32),
) -> (Outcome, String) {
    match analyze(job, toc, db, progress) {
        Ok(a) => finish(job, &a, dest),
        Err(e) => failed(&e),
    }
}

/// The outcome of an [`analyze`] that failed.
pub fn failed(e: &anyhow::Error) -> (Outcome, String) {
    (Outcome::Failed, format!("Repair check failed: {e:#}"))
}

/// The second half of [`run`]: writes repaired copies to `dest` if possible.
pub fn finish(job: &AlbumJob, analysis: &Analysis, dest: Option<&Path>) -> (Outcome, String) {
    let summary = analysis.summary();
    match (dest, analysis.outcome()) {
        (Some(dest), Outcome::Repairable) => {
            match write_repaired(job, analysis, dest, &mut |_| {}) {
                Ok((files, encoder)) => (
                    Outcome::Repaired,
                    format!(
                        "{summary}. Wrote {} repaired file{} to {} ({encoder})",
                        files.len(),
                        if files.len() == 1 { "" } else { "s" },
                        dest.display()
                    ),
                ),
                Err(e) => (
                    Outcome::Repairable,
                    format!("{summary}. Writing failed: {e:#}"),
                ),
            }
        }
        (None, Outcome::Repairable) => (
            Outcome::Repairable,
            format!("{summary}; press r to write a repaired copy"),
        ),
        (_, outcome) => (outcome, summary),
    }
}

/// Where repaired copies of an album in `dir` go: `out` mirrors the library.
pub fn destination(out: &Path, root: &Path, dir: &Path) -> PathBuf {
    out.join(dir.strip_prefix(root).unwrap_or(dir))
}

/// Default folder for repaired copies.
pub fn default_out() -> PathBuf {
    std::env::home_dir()
        .unwrap_or_default()
        .join("accuraterip-repaired")
}

/// Writes corrected copies of the affected tracks into `out_dir`. Returns
/// the files written and the encoder used.
pub fn write_repaired(
    job: &AlbumJob,
    analysis: &Analysis,
    out_dir: &Path,
    progress: &mut dyn FnMut(f32),
) -> Result<(Vec<PathBuf>, &'static str)> {
    let (_, _, fixes) = analysis
        .plan
        .as_ref()
        .ok_or_else(|| anyhow!("nothing to repair"))?;
    let bounds = &analysis.bounds;
    let tracks = affected_tracks(bounds, fixes);
    std::fs::create_dir_all(out_dir)?;
    // Repaired copies must never replace the originals.
    let out_canon = std::fs::canonicalize(out_dir)?;
    for t in &job.tracks {
        if let Some(parent) = t.path.parent()
            && std::fs::canonicalize(parent).is_ok_and(|p| p == out_canon)
        {
            bail!(
                "the output folder {} is the album's own folder; choose a different --repair folder",
                out_dir.display()
            );
        }
    }
    let mut written = Vec::new();
    let mut encoder = "";
    for (n, &number) in tracks.iter().enumerate() {
        let k = number as usize - 1;
        let track = &job.tracks[k];
        let (start, end) = (bounds[k], bounds[k + 1]);
        let mut pcm: Vec<i16> = Vec::with_capacity(((end - start) * 2) as usize);
        decode::decode(&track.path, false, |frames| {
            for &f in frames {
                pcm.push(f as u16 as i16);
                pcm.push((f >> 16) as u16 as i16);
            }
        })?;
        let first_word = (start * 2) as usize;
        for f in fixes
            .iter()
            .filter(|f| f.word >= first_word && f.word < (end * 2) as usize)
        {
            let i = f.word - first_word;
            pcm[i] = (pcm[i] as u16 ^ f.diff) as i16;
        }
        let name = track
            .path
            .file_name()
            .ok_or_else(|| anyhow!("bad file name"))?;
        let out = out_dir.join(name);
        encoder = flacout::write_track(&track.path, &out, &pcm)?;
        written.push(out);
        progress((n + 1) as f32 / tracks.len() as f32);
    }
    Ok((written, encoder))
}

/// IEEE CRC32, as used by CTDB.
struct Crc32 {
    reg: u32,
}

fn crc_table() -> &'static [u32; 256] {
    static T: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, v) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xedb8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *v = c;
        }
        t
    })
}

impl Crc32 {
    fn new() -> Crc32 {
        Crc32 { reg: 0xffff_ffff }
    }

    fn update(&mut self, bytes: &[u8]) {
        self.reg = crc32_raw(self.reg, bytes);
    }

    fn value(&self) -> u32 {
        !self.reg
    }
}

/// CRC register update without pre/post conditioning.
fn crc32_raw(mut reg: u32, bytes: &[u8]) -> u32 {
    let t = crc_table();
    for &b in bytes {
        reg = t[((reg ^ b as u32) & 0xff) as usize] ^ (reg >> 8);
    }
    reg
}

/// Advances a CRC as if `len` zero bytes followed (zlib's `crc32_combine`
/// with an all-zero second part).
fn crc32_shift(mut crc: u32, mut len: u64) -> u32 {
    fn times(mat: &[u32; 32], mut vec: u32) -> u32 {
        let mut sum = 0;
        let mut i = 0;
        while vec != 0 {
            if vec & 1 != 0 {
                sum ^= mat[i];
            }
            vec >>= 1;
            i += 1;
        }
        sum
    }
    fn square(mat: &[u32; 32]) -> [u32; 32] {
        let mut sq = [0u32; 32];
        for n in 0..32 {
            sq[n] = times(mat, mat[n]);
        }
        sq
    }
    if len == 0 {
        return crc;
    }
    let mut odd = [0u32; 32];
    odd[0] = 0xedb8_8320;
    for (n, v) in odd.iter_mut().enumerate().skip(1) {
        *v = 1 << (n - 1);
    }
    let mut even = square(&odd);
    odd = square(&even);
    loop {
        even = square(&odd);
        if len & 1 != 0 {
            crc = times(&even, crc);
        }
        len >>= 1;
        if len == 0 {
            break;
        }
        odd = square(&even);
        if len & 1 != 0 {
            crc = times(&odd, crc);
        }
        len >>= 1;
        if len == 0 {
            break;
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crc(bytes: &[u8]) -> u32 {
        let mut c = Crc32::new();
        c.update(bytes);
        c.value()
    }

    #[test]
    fn crc32_known_value() {
        assert_eq!(crc(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn crc32_segment_from_prefixes() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
        let (a, b) = (123, 871);
        let seg = crc(&data[a..b]);
        let derived = crc(&data[..b]) ^ crc32_shift(crc(&data[..a]), (b - a) as u64);
        assert_eq!(derived, seg);
    }

    #[test]
    fn crc32_of_corrected_data() {
        let data: Vec<u8> = (0..400u32).map(|i| (i * 13) as u8).collect();
        let mut fixed = data.clone();
        fixed[100] ^= 0x5a;
        fixed[101] ^= 0x01;
        let diff = crc32_raw(0, &[0x5a, 0x01]);
        assert_eq!(
            crc(&fixed),
            crc(&data) ^ crc32_shift(diff, (400 - 102) as u64)
        );
    }
}
