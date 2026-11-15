// SPDX-License-Identifier: GPL-3.0-or-later

//! Repairing a sample slip with CTDB parity data.
//!
//! A drive that loses sync part-way through a track drops or repeats a few
//! samples, so all the audio after that point is shifted. AccurateRip shows
//! it as accurate tracks on either side matching at offsets a few samples
//! apart. CTDB's parity covers the whole disc at one alignment, so as ripped
//! everything on one side of the slip is wrong: far more than it can correct.
//!
//! Realigned at the slip point, though, only the samples around it are
//! wrong, which the parity can correct. So:
//!
//! 1. Find the slip point. The parity is split into columns of every
//!    `stride`-th word. For each candidate point (at intervals much shorter
//!    than a stride) the syndromes of a sample of columns of the realigned
//!    album are compared with CTDB's: a column without a wrong word matches
//!    one of CTDB's exactly. Far from the slip point, every column has a
//!    wrong word; near it, most have none.
//! 2. Check the album realigned there against CTDB as usual (see
//!    [`repair`]), which finds the corrections and confirms them with CTDB's
//!    CRC32.
//! 3. Write the realigned, corrected tracks on the side that was moved.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::db::{self, Db};
use crate::decode;
use crate::disc::SECTOR_FRAMES;
use crate::flacout;
use crate::parity::{self, Fix, Geometry, MAX_NPAR};
use crate::repair::{self, Outcome};
use crate::scan::AlbumJob;

/// A sample slip, as AccurateRip shows it: tracks `first..=last` (indices)
/// don't match, and the accurate tracks around them match at offsets
/// `shift` frames apart. With `last + 1 == first`, no track is bad: the slip
/// is where track `first - 1` meets track `first`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slip {
    pub first: usize,
    pub last: usize,
    /// The offset of the tracks after the slip minus that of those before.
    pub shift: i64,
}

/// Columns sampled to find the slip point, in frames (two words each).
const SAMPLE_FRAMES: usize = 64;

/// Sampled columns that must match CTDB's to accept a slip point.
const MIN_VOTES: u32 = SAMPLE_FRAMES as u32 / 4;

/// The album realigned at frame `at`: its frame `i` is the rip's frame
/// `i + head` before `at` and `i + tail` from `at` on. One of the two is 0.
#[derive(Debug, Clone, Copy)]
struct Splice {
    at: u64,
    head: i64,
    tail: i64,
}

impl Splice {
    /// The rip's frame that the realigned album's frame `i` comes from.
    fn source(&self, i: u64) -> i64 {
        i as i64 + if i < self.at { self.head } else { self.tail }
    }
}

/// Which side of the slip to move, as `(head, tail)` shifts (see
/// [`Splice`]), and the tracks that then change: the side with fewer tracks
/// outside the slip, the tail if it's a tie.
fn sides(slip: Slip, tracks: usize) -> (i64, i64, std::ops::Range<usize>) {
    let before = slip.first;
    let after = tracks - 1 - slip.last;
    match after <= before {
        true => (0, slip.shift, slip.first..tracks),
        false => (-slip.shift, 0, 0..slip.last + 1),
    }
}

/// Turns the rip's frames, fed in order, into the realigned album's.
/// Frames from outside the rip are silence.
struct Spliced {
    splice: Splice,
    total: u64,
    /// The rip's frames from `start` on, up to `have`.
    buf: VecDeque<u32>,
    start: u64,
    have: u64,
    /// The realigned album's next frame.
    next: u64,
    out: Vec<u32>,
}

impl Spliced {
    fn new(splice: Splice, total: u64) -> Spliced {
        Spliced {
            splice,
            total,
            buf: VecDeque::new(),
            start: 0,
            have: 0,
            next: 0,
            out: Vec::new(),
        }
    }

    /// Feeds the rip's next frames, handing `sink` the realigned album's
    /// frames that are then known.
    fn push(&mut self, frames: &[u32], sink: &mut impl FnMut(&[u32])) {
        self.buf.extend(frames);
        self.have += frames.len() as u64;
        self.emit(false, sink);
    }

    /// Hands `sink` the rest of the realigned album, once the rip is all fed.
    fn finish(&mut self, sink: &mut impl FnMut(&[u32])) {
        self.emit(true, sink);
    }

    fn emit(&mut self, all: bool, sink: &mut impl FnMut(&[u32])) {
        self.out.clear();
        while self.next < self.total {
            let src = self.splice.source(self.next);
            if !all && src >= self.have as i64 {
                break;
            }
            let frame = u64::try_from(src)
                .ok()
                .and_then(|s| s.checked_sub(self.start))
                .and_then(|i| self.buf.get(i as usize).copied())
                .unwrap_or(0);
            self.out.push(frame);
            self.next += 1;
        }
        if !self.out.is_empty() {
            sink(&self.out);
        }
        // Later frames come from no earlier than this.
        let keep = (self.next as i64 + self.splice.head.min(self.splice.tail)).max(0) as u64;
        let drop = keep.saturating_sub(self.start).min(self.buf.len() as u64);
        self.buf.drain(..drop as usize);
        self.start += drop;
    }
}

/// The words of the sampled columns, for the album realigned anywhere: for
/// column `j` (word column `cols[j]`) and row `r` (stride `r + 1`), word
/// `[j * rows + r]` of `head` if the realigned album takes it from before
/// the slip point, else of `tail`.
struct Samples {
    cols: Vec<usize>,
    rows: usize,
    head: Vec<u16>,
    tail: Vec<u16>,
}

impl Samples {
    /// The frame that row `r` of column `j` is in.
    fn frame(&self, geo: Geometry, j: usize, r: usize) -> u64 {
        ((geo.stride * (r + 1) + self.cols[j]) / 2) as u64
    }
}

/// Decodes the album, keeping the sampled columns' words.
fn gather(
    job: &AlbumJob,
    geo: Geometry,
    head: i64,
    tail: i64,
    progress: &mut dyn FnMut(f32),
) -> Result<Samples> {
    let total = (geo.words / 2) as u64;
    let rows = geo.stridecount;
    // Frame columns spread over the stride; both words of each.
    let frame_cols: Vec<usize> = (0..SAMPLE_FRAMES)
        .map(|k| k * geo.stride / 2 / SAMPLE_FRAMES)
        .collect();
    let cols: Vec<usize> = frame_cols
        .iter()
        .flat_map(|&f| [2 * f, 2 * f + 1])
        .collect();
    // The rip's frames wanted, with where each goes: (frame, slot), slot
    // `(k * rows + r) * 2 + side` for frame column `k`.
    let mut wants: Vec<(u64, usize)> = Vec::with_capacity(SAMPLE_FRAMES * rows * 2);
    for (k, &fc) in frame_cols.iter().enumerate() {
        for r in 0..rows {
            let i = (geo.stride / 2 * (r + 1) + fc) as i64;
            for (side, shift) in [head, tail].into_iter().enumerate() {
                if let Ok(src) = u64::try_from(i + shift)
                    && src < total
                {
                    wants.push((src, (k * rows + r) * 2 + side));
                }
            }
        }
    }
    wants.sort_unstable();
    let mut frames_at = vec![0u32; SAMPLE_FRAMES * rows * 2];
    let mut next = 0;
    let notes = decode::decode_album(&job.tracks, false, |pos, frames| {
        let end = pos + frames.len() as u64;
        while let Some(&(src, slot)) = wants.get(next)
            && src < end
        {
            if let Some(i) = src.checked_sub(pos) {
                frames_at[slot] = frames[i as usize];
            }
            next += 1;
        }
        progress(end as f32 / total as f32);
    });
    check_notes(&notes)?;
    let (mut head_words, mut tail_words) = (Vec::new(), Vec::new());
    for k in 0..SAMPLE_FRAMES {
        for half in 0..2 {
            for r in 0..rows {
                let slot = (k * rows + r) * 2;
                head_words.push((frames_at[slot] >> (16 * half)) as u16);
                tail_words.push((frames_at[slot + 1] >> (16 * half)) as u16);
            }
        }
    }
    Ok(Samples {
        cols,
        rows,
        head: head_words,
        tail: tail_words,
    })
}

/// Fails on a track that didn't decode in full.
fn check_notes(notes: &[Option<String>]) -> Result<()> {
    match notes.iter().enumerate().find_map(|(k, n)| {
        n.as_ref()
            .filter(|n| *n != decode::MD5_MISMATCH)
            .map(|n| (k, n))
    }) {
        Some((k, note)) => bail!("track {}: {note}", k + 1),
        None => Ok(()),
    }
}

/// A syndrome as a fixed-size key.
fn key(syn: &[u16]) -> [u16; MAX_NPAR] {
    std::array::from_fn(|i| syn.get(i).copied().unwrap_or(0))
}

/// Finds the slip point in frames `region`, given an entry's full syndrome
/// (`[stride][npar]`): the candidate point at which the most sampled
/// columns match CTDB's columns at one offset.
fn locate(
    samples: &Samples,
    geo: Geometry,
    region: (u64, u64),
    db_syn: &[u16],
    npar: usize,
) -> Option<u64> {
    // CTDB's columns by syndrome; one shared by several columns says nothing.
    let mut columns: HashMap<[u16; MAX_NPAR], usize> = HashMap::new();
    for (c, syn) in db_syn.chunks_exact(npar).enumerate() {
        if syn.iter().any(|&s| s != 0) {
            columns
                .entry(key(syn))
                .and_modify(|v| *v = usize::MAX)
                .or_insert(c);
        }
    }
    let rows = samples.rows;
    let ncols = samples.cols.len();
    // Candidates `stride / 4` frames apart: one is within `stride / 8`
    // frames of the slip point, so that at most a quarter of the columns
    // (`stride` words) has a misaligned word there.
    let step = (geo.stride / 4).max(1) as u64;
    let (from, to) = region;
    let mut syn = vec![0u16; ncols * npar];
    for j in 0..ncols {
        let words = (0..rows).map(|r| {
            let i = j * rows + r;
            match samples.frame(geo, j, r) < from {
                true => samples.head[i],
                false => samples.tail[i],
            }
        });
        parity::column_syndrome(&mut syn[j * npar..(j + 1) * npar], words);
    }
    let mut best: Option<(u32, u64)> = None;
    let mut votes: HashMap<usize, u32> = HashMap::new();
    let mut at = from;
    loop {
        votes.clear();
        for (j, s) in syn.chunks_exact(npar).enumerate() {
            if let Some(&c) = columns.get(&key(s))
                && c != usize::MAX
            {
                let delta = (c + geo.stride - samples.cols[j]) % geo.stride;
                *votes.entry(delta).or_default() += 1;
            }
        }
        let score = votes.values().copied().max().unwrap_or(0);
        if best.is_none_or(|(s, _)| score > s) {
            best = Some((score, at));
        }
        if at >= to {
            break;
        }
        // Rows in frames `at..next` now come from before the slip point.
        let next = (at + step).min(to);
        for j in 0..ncols {
            let col = samples.cols[j] as u64;
            let stride = geo.stride as u64;
            // The first row at or after frame `at`.
            let first = (2 * at).saturating_sub(stride + col).div_ceil(stride) as usize;
            for r in first..rows {
                if samples.frame(geo, j, r) >= next {
                    break;
                }
                let i = j * rows + r;
                let diff = samples.head[i] ^ samples.tail[i];
                if diff != 0 {
                    parity::change_row(&mut syn[j * npar..(j + 1) * npar], rows, r, diff);
                }
            }
        }
        at = next;
    }
    best.filter(|&(score, _)| score >= MIN_VOTES)
        .map(|(_, at)| at)
}

/// Checks the album realigned at a sample slip against CTDB's parity and,
/// if that matches or is repairable and `dest` is given, writes corrected
/// copies of the realigned tracks there.
pub fn run(
    job: &AlbumJob,
    toc: &str,
    db: &Db,
    slip: Slip,
    dest: Option<&Path>,
    progress: &mut dyn FnMut(f32),
) -> (Outcome, String) {
    attempt(job, toc, db, slip, dest, progress).unwrap_or_else(|e| repair::failed(&e))
}

fn attempt(
    job: &AlbumJob,
    toc: &str,
    db: &Db,
    slip: Slip,
    dest: Option<&Path>,
    progress: &mut dyn FnMut(f32),
) -> Result<(Outcome, String)> {
    let n = job.tracks.len();
    if slip.first == 0 || slip.first > slip.last + 1 || slip.last >= n || slip.shift == 0 {
        bail!("no sample slip to repair");
    }
    let bounds = repair::track_bounds(job)?;
    let total = bounds[n];
    let number = |k: usize| job.tracks[k].number.unwrap_or(k as u32 + 1);
    // The tracks to re-rip: those that don't match, or both sides of a
    // slip between tracks.
    let bad: Vec<usize> = match slip.first > slip.last {
        true => vec![slip.last, slip.first],
        false => (slip.first..=slip.last).collect(),
    };
    let rerip = format!(
        "re-rip track{} {}",
        plural(bad.len()),
        bad.iter()
            .map(|&k| number(k).to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let not = |why: &str| {
        Ok((
            Outcome::NotRepairable,
            format!("Not repairable: {why}; {rerip}"),
        ))
    };
    let mut entries: Vec<_> = db::ctdb_exact(db, toc)
        .context("CTDB lookup")?
        .into_iter()
        .filter(|e| e.parity_url.is_some() && e.crc32.is_some())
        .collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.confidence));
    let Some(stride_frames) = entries.first().map(repair::entry_stride) else {
        return not("CTDB has no repair data for this disc");
    };
    entries.retain(|e| repair::entry_stride(e) == stride_frames);
    let geo = Geometry::new(total, stride_frames * 2);
    if geo.stridecount == 0 {
        return not("the disc is too short for its repair data");
    }
    let (head, tail, moved) = sides(slip, n);

    let samples = gather(job, geo, head, tail, &mut |f| progress(f / 3.0))?;
    let region = (bounds[slip.first], bounds[slip.last + 1]);
    let mut at = None;
    for e in &entries {
        let Some(url) = &e.parity_url else { continue };
        let npar = repair::entry_npar(e);
        let bytes = db::fetch(db, url).context("downloading CTDB parity")?;
        let Some(syn) = parity::bytes_to_syndrome(geo.stride, npar, &bytes) else {
            continue;
        };
        at = locate(&samples, geo, region, &syn, npar);
        if at.is_some() {
            break;
        }
    }
    let Some(at) = at else {
        return not("couldn't find where the drive lost sync in CTDB's repair data");
    };
    let splice = Splice { at, head, tail };

    let mut pass = repair::Pass::new(job, toc, db)?;
    let mut spliced = Spliced::new(splice, total);
    let notes = decode::decode_album(&job.tracks, false, |pos, frames| {
        spliced.push(frames, &mut |f| pass.push(f));
        progress((1.0 + (pos + frames.len() as u64) as f32 / total as f32) / 3.0);
    });
    spliced.finish(&mut |f| pass.push(f));
    let analysis = pass.finish(&notes, db)?;
    let Some((confidence, _, fixes)) = analysis.fix_plan() else {
        let why = analysis.summary();
        let why = why.trim_start_matches("Not repairable: ");
        return not(&format!("realigned at the slip, {why}"));
    };

    // Where the slip is, and what it did.
    let k = bounds.partition_point(|&b| b <= at).clamp(1, n) - 1;
    let secs = (at - bounds[k]) / (SECTOR_FRAMES * 75);
    let place = match (k, secs) {
        (1.., 0) if at == bounds[k] => {
            format!("between tracks {} and {}", number(k - 1), number(k))
        }
        _ => format!("in track {} at {}:{:02}", number(k), secs / 60, secs % 60),
    };
    let what = match slip.shift < 0 {
        true => "dropped",
        false => "repeated",
    };
    // The moved tracks, and any others the corrections change.
    let mut tracks: Vec<usize> = moved.collect();
    for t in repair::affected_tracks(&bounds, fixes) {
        let t = t as usize - 1;
        if !tracks.contains(&t) {
            tracks.push(t);
        }
    }
    tracks.sort_unstable();
    let list = tracks
        .iter()
        .map(|&k| number(k).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let summary = format!(
        "Repairable: the drive {what} {} samples {place}; realigning \
         track{} {list} and correcting {} samples matches CTDB (confidence {confidence})",
        slip.shift.abs(),
        plural(tracks.len()),
        fixes.len(),
    );
    let Some(dest) = dest else {
        return Ok((
            Outcome::Repairable,
            format!("{summary}; press r to write a repaired copy"),
        ));
    };
    Ok(
        match write(job, &bounds, splice, fixes, &tracks, dest, &mut |f| {
            progress((2.0 + f) / 3.0)
        }) {
            Ok((files, encoder)) => (
                Outcome::Repaired,
                format!(
                    "{summary}. Wrote {} repaired file{} to {} ({encoder})",
                    files.len(),
                    plural(files.len()),
                    dest.display()
                ),
            ),
            Err(e) => (
                Outcome::Repairable,
                format!("{summary}. Writing failed: {e:#}"),
            ),
        },
    )
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Writes the files holding `tracks` (indices) of the album realigned by
/// `splice` and corrected by `fixes` into `out_dir`. Returns the files
/// written and the encoder used.
fn write(
    job: &AlbumJob,
    bounds: &[u64],
    splice: Splice,
    fixes: &[Fix],
    tracks: &[usize],
    out_dir: &Path,
    progress: &mut dyn FnMut(f32),
) -> Result<(Vec<PathBuf>, &'static str)> {
    repair::prepare_out_dir(job, out_dir)?;
    let files = repair::file_ranges(job, tracks.iter().copied());
    let total = bounds[job.tracks.len()];
    // The file being collected: (index in `files`, its samples).
    let mut current: Option<(usize, Vec<i16>)> = None;
    let mut next_file = 0;
    let mut staged = Vec::new();
    let mut encoder = "";
    let mut failure: Option<anyhow::Error> = None;
    let mut pos = 0u64;
    let mut fix = 0;
    // Takes the realigned album's next frames.
    let mut take = |frames: &[u32]| -> Result<()> {
        for &f in frames {
            let mut f = f;
            while let Some(x) = fixes.get(fix).filter(|x| (x.word / 2) as u64 == pos) {
                f ^= (x.diff as u32) << (x.word % 2 * 16);
                fix += 1;
            }
            if current.is_none()
                && let Some(&(first, _)) = files.get(next_file)
                && bounds[first] == pos
            {
                current = Some((next_file, start_file(job, bounds, files[next_file])?));
                next_file += 1;
            }
            if let Some((i, pcm)) = &mut current {
                pcm.push(f as u16 as i16);
                pcm.push((f >> 16) as u16 as i16);
                let (first, last) = files[*i];
                if pos + 1 == bounds[last] {
                    let track = &job.tracks[first];
                    let name = track
                        .path
                        .file_name()
                        .ok_or_else(|| anyhow!("bad file name"))?;
                    let (file, enc) = flacout::stage_track(&track.path, &out_dir.join(name), pcm)?;
                    encoder = enc;
                    staged.push(file);
                    current = None;
                    progress(next_file as f32 / files.len() as f32);
                }
            }
            pos += 1;
        }
        Ok(())
    };
    let mut sink = |frames: &[u32]| {
        if failure.is_none()
            && let Err(e) = take(frames)
        {
            failure = Some(e);
        }
    };
    let mut spliced = Spliced::new(splice, total);
    let notes = decode::decode_album(&job.tracks, false, |_, frames| {
        spliced.push(frames, &mut sink);
    });
    spliced.finish(&mut sink);
    drop(sink);
    check_notes(&notes)?;
    if let Some(e) = failure {
        return Err(e);
    }
    if staged.len() != files.len() {
        bail!("the album decoded short");
    }
    let written = staged
        .into_iter()
        .map(flacout::Staged::commit)
        .collect::<Result<Vec<_>>>()?;
    Ok((written, encoder))
}

/// The start of a file to be rewritten that holds tracks `first..last`:
/// room for its samples, and an image's pregap, which precedes the album.
fn start_file(job: &AlbumJob, bounds: &[u64], (first, last): (usize, usize)) -> Result<Vec<i16>> {
    let track = &job.tracks[first];
    let skip = track.image_start.unwrap_or(0);
    let len = usize::try_from((skip + bounds[last] - bounds[first]) * 2)
        .context("file too large to repair in memory")?;
    let mut pcm: Vec<i16> = Vec::new();
    pcm.try_reserve_exact(len)
        .context("not enough memory to repair this file")?;
    if skip > 0 {
        let mut left = skip as usize;
        decode::decode(&track.path, false, |frames| {
            for &f in &frames[..left.min(frames.len())] {
                pcm.push(f as u16 as i16);
                pcm.push((f >> 16) as u16 as i16);
            }
            left -= left.min(frames.len());
        })?;
        if left > 0 {
            bail!("{} decoded short", track.path.display());
        }
    }
    Ok(pcm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, seed: u32) -> Vec<u32> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x
            })
            .collect()
    }

    fn splice_all(rip: &[u32], splice: Splice, chunk: usize) -> Vec<u32> {
        let mut s = Spliced::new(splice, rip.len() as u64);
        let mut out = Vec::new();
        for c in rip.chunks(chunk) {
            s.push(c, &mut |f| out.extend_from_slice(f));
        }
        s.finish(&mut |f| out.extend_from_slice(f));
        out
    }

    #[test]
    fn splices_both_ways() {
        let rip: Vec<u32> = (1..=50).collect();
        for chunk in [1, 3, 7, 50] {
            // Six frames dropped at 20: the tail moves later.
            let s = splice_all(
                &rip,
                Splice {
                    at: 20,
                    head: 0,
                    tail: -6,
                },
                chunk,
            );
            assert_eq!(s.len(), 50);
            assert_eq!(&s[..20], &rip[..20]);
            assert_eq!(&s[20..], &rip[14..44]);
            // Repeated, moving the head: frames before 20 come 6 later, and
            // the first 6 aren't in the rip.
            let s = splice_all(
                &rip,
                Splice {
                    at: 20,
                    head: 6,
                    tail: 0,
                },
                chunk,
            );
            assert_eq!(&s[..14], &rip[6..20]);
            assert_eq!(&s[14..20], &rip[20..26]);
            assert_eq!(&s[20..], &rip[20..]);
            let s = splice_all(
                &rip,
                Splice {
                    at: 20,
                    head: -6,
                    tail: 0,
                },
                chunk,
            );
            assert_eq!(&s[..6], &[0; 6]);
            assert_eq!(&s[6..20], &rip[..14]);
            assert_eq!(&s[20..], &rip[20..]);
        }
    }

    /// A disc ripped with `shift` frames lost (negative) or repeated at
    /// frame `slip`: the true audio, and the rip.
    fn slipped(len: usize, slip: usize, shift: i64) -> (Vec<u32>, Vec<u32>) {
        let disc = noise(len + 100, 7);
        let truth = disc[..len].to_vec();
        let mut rip = truth[..slip].to_vec();
        let after = (slip as i64 - shift) as usize;
        rip.extend_from_slice(&disc[after..after + len - slip]);
        (truth, rip)
    }

    #[test]
    fn locates_slip_point() {
        use crate::parity::ParityEncoder;
        let stride = 512; // words
        let npar = 8;
        let len = stride / 2 * 60;
        for (slip, shift) in [(9000, -6), (9000, 6), (5003, -40)] {
            let (truth, rip) = slipped(len, slip, shift);
            let geo = Geometry::new(len as u64, stride);
            let mut enc = ParityEncoder::new(geo);
            enc.push(&truth);
            let db_syn = enc.syndrome(npar, stride, 0);
            // Gather as `gather` does, from memory.
            let (head, tail) = (0, shift);
            let rows = geo.stridecount;
            let frame_cols: Vec<usize> = (0..SAMPLE_FRAMES)
                .map(|k| k * stride / 2 / SAMPLE_FRAMES)
                .collect();
            let cols: Vec<usize> = frame_cols
                .iter()
                .flat_map(|&f| [2 * f, 2 * f + 1])
                .collect();
            let at = |i: i64| usize::try_from(i).ok().and_then(|i| rip.get(i)).copied();
            let (mut hw, mut tw) = (Vec::new(), Vec::new());
            for &fc in &frame_cols {
                for half in 0..2 {
                    for r in 0..rows {
                        let i = (stride / 2 * (r + 1) + fc) as i64;
                        hw.push((at(i + head).unwrap_or(0) >> (16 * half)) as u16);
                        tw.push((at(i + tail).unwrap_or(0) >> (16 * half)) as u16);
                    }
                }
            }
            let samples = Samples {
                cols,
                rows,
                head: hw,
                tail: tw,
            };
            let found = locate(&samples, geo, (4000, 12000), &db_syn, npar).expect("found");
            assert!(
                found.abs_diff(slip as u64) <= stride as u64 / 4,
                "slip {slip} shift {shift}: found {found}"
            );
            // Realigned there, the rip is the truth but for a few frames
            // around the slip, which the parity corrects.
            let s = splice_all(
                &rip,
                Splice {
                    at: found,
                    head,
                    tail,
                },
                1000,
            );
            let mut enc = ParityEncoder::new(geo);
            enc.push(&s);
            let fixes = parity::compute_fixes(&enc, &db_syn, npar, 0).expect("repairable");
            let mut fixed = s.clone();
            for f in &fixes {
                fixed[f.word / 2] ^= (f.diff as u32) << (f.word % 2 * 16);
            }
            let (a, b) = parity::crc_window(geo, 0);
            assert_eq!(fixed[a as usize..b as usize], truth[a as usize..b as usize]);
        }
    }

    #[test]
    fn moves_the_shorter_side() {
        let slip = Slip {
            first: 1,
            last: 1,
            shift: -6,
        };
        assert_eq!(sides(slip, 10), (6, 0, 0..2));
        assert_eq!(sides(slip, 3), (0, -6, 1..3));
        let late = Slip {
            first: 7,
            last: 7,
            shift: -6,
        };
        assert_eq!(sides(late, 10), (0, -6, 7..10));
    }
}
