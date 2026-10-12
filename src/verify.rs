// SPDX-License-Identifier: GPL-3.0-or-later

//! Verifying one album against AccurateRip.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use serde::{Deserialize, Serialize};

use crate::crc::{Frame450Windows, OffsetAccum, TrackCrc};
use crate::db::{self, Db, Entry, Pressing, TocLayout};
use crate::decode;
use crate::disc::{self, DiscId, SECTOR_FRAMES};
use crate::repair::{self, Outcome};
use crate::scan::AlbumJob;

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub min_confidence: u32,
    pub use_tags: bool,
    pub offset_range: i64,
    pub max_candidates: usize,
    /// Ask CTDB for the data track of Enhanced CDs missing from AccurateRip.
    pub use_ctdb: bool,
    /// Check against CTDB first; ask AccurateRip only about albums that
    /// don't match it.
    pub ctdb_first: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Verdict {
    Corrupted,
    RipErrors,
    Accurate,
    CannotCheck,
}

impl Verdict {
    pub const ALL: [Verdict; 4] = [
        Verdict::Corrupted,
        Verdict::RipErrors,
        Verdict::Accurate,
        Verdict::CannotCheck,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Verdict::Corrupted => "Tracks Corrupted",
            Verdict::RipErrors => "Ripping Errors",
            Verdict::Accurate => "Accurately Ripped",
            Verdict::CannotCheck => "Cannot Check",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackStatus {
    Accurate,
    /// Accurate according to a dBpoweramp `ACCURATERIPRESULT` tag.
    AccurateTag,
    /// The whole rip matches a CTDB entry (`--invert`).
    AccurateCtdb,
    LowConfidence,
    Mismatch,
    NotInDb,
    Corrupted,
    Unchecked,
}

impl TrackStatus {
    pub fn label(self) -> &'static str {
        match self {
            TrackStatus::Accurate => "Accurate",
            TrackStatus::AccurateTag => "Accurate (tag)",
            TrackStatus::AccurateCtdb => "Accurate (CTDB)",
            TrackStatus::LowConfidence => "Low confidence",
            TrackStatus::Mismatch => "Mismatch",
            TrackStatus::NotInDb => "Not in DB",
            TrackStatus::Corrupted => "Corrupted",
            TrackStatus::Unchecked => "-",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackResult {
    pub number: u32,
    pub path: PathBuf,
    pub v1: Option<u32>,
    pub v2: Option<u32>,
    /// The matching DB checksum, or the most confident one on a mismatch.
    pub db_crc: Option<u32>,
    pub confidence: u32,
    pub offset: i64,
    pub status: TrackStatus,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlbumResult {
    pub title: String,
    pub dir: PathBuf,
    pub verdict: Verdict,
    pub reason: String,
    pub disc_id: Option<String>,
    pub url: Option<String>,
    pub pressings: usize,
    pub tracks: Vec<TrackResult>,
    /// The result may change on a later run (database updates, corruption
    /// that might be a transient read error), so it must not be cached.
    pub recheck: bool,
    /// Loaded from the results cache rather than verified this run.
    #[serde(default)]
    pub cached: bool,
    /// MusicBrainz disc ID of the reconstructed TOC.
    #[serde(default)]
    pub mb_disc_id: Option<String>,
    /// A `MUSICBRAINZ_DISCID` tag that differs from `mb_disc_id`.
    #[serde(default)]
    pub mb_tag_mismatch: Option<String>,
    /// Set when the disc was identified as an Enhanced CD via CTDB.
    #[serde(default)]
    pub data_track: Option<String>,
    /// For albums with ripping errors: whether CTDB knows the disc and has
    /// repair data for it.
    #[serde(default)]
    pub ctdb_repair: Option<String>,
    /// CTDB has entries for this disc (so a parity check is possible).
    #[serde(default)]
    pub ctdb_known: bool,
    /// Result of the CTDB parity check, if one ran.
    #[serde(default)]
    pub repair_outcome: Option<crate::repair::Outcome>,
    /// The disc's TOC in CTDB's notation (real layout, as far as known).
    #[serde(default)]
    pub ctdb_toc: Option<String>,
    /// Outcome of a CTDB repair attempt.
    #[serde(default)]
    pub repair: Option<String>,
    /// Explanation of a ripping error, e.g. a sample slip.
    #[serde(default)]
    pub diagnosis: Option<String>,
    /// Checked with `--invert` (CTDB first).
    #[serde(default)]
    pub ctdb_first: bool,
}

impl AlbumResult {
    pub fn musicbrainz_url(&self) -> Option<String> {
        self.mb_disc_id
            .as_ref()
            .map(|id| format!("https://musicbrainz.org/cdtoc/{id}"))
    }
}

pub type Progress<'a> = &'a mut dyn FnMut(f32);

/// A result with every track unchecked.
fn blank_result(job: &AlbumJob) -> AlbumResult {
    AlbumResult {
        title: job.title.clone(),
        dir: job.dir.clone(),
        verdict: Verdict::CannotCheck,
        reason: String::new(),
        disc_id: None,
        url: None,
        pressings: 0,
        recheck: false,
        cached: false,
        mb_disc_id: None,
        mb_tag_mismatch: None,
        data_track: None,
        ctdb_repair: None,
        ctdb_known: false,
        repair_outcome: None,
        ctdb_toc: None,
        repair: None,
        diagnosis: None,
        ctdb_first: false,
        tracks: job
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| TrackResult {
                number: t.number.unwrap_or(i as u32 + 1),
                path: t.path.clone(),
                v1: None,
                v2: None,
                db_crc: None,
                confidence: 0,
                offset: 0,
                status: TrackStatus::Unchecked,
                note: t.problem.clone(),
            })
            .collect(),
    }
}

/// Track lengths in sectors of an eligible album; sets the audio-only disc IDs.
fn identify(job: &AlbumJob, result: &mut AlbumResult) -> Vec<u32> {
    let sectors: Vec<u32> = job
        .tracks
        .iter()
        .map(|t| (t.frames.unwrap() / SECTOR_FRAMES) as u32)
        .collect();
    let id = DiscId::from_track_sectors(&sectors);
    result.disc_id = Some(id.file_name());
    result.url = Some(id.url());
    set_mb_id(job, result, &sectors, 0);
    sectors
}

/// Sets the MusicBrainz disc ID for track 1 at `start`, flagging a differing tag.
fn set_mb_id(job: &AlbumJob, result: &mut AlbumResult, sectors: &[u32], start: u32) {
    let mb_id = disc::musicbrainz_id(sectors, start);
    result.mb_tag_mismatch = job
        .tracks
        .iter()
        .filter_map(|t| t.mb_disc_id.as_ref())
        .find(|t| **t != mb_id)
        .cloned();
    result.mb_disc_id = Some(mb_id);
}

pub fn verify(job: &AlbumJob, db: &Db, opts: &Options, progress: Progress<'_>) -> AlbumResult {
    let mut result = blank_result(job);
    if let Err(reason) = check_eligible(job) {
        result.reason = reason;
        return result;
    }
    let sectors = identify(job, &mut result);

    if opts.use_tags {
        let tagged: Option<Vec<u32>> = job
            .tracks
            .iter()
            .map(|t| t.ar_result.as_deref().and_then(tag_confidence))
            .collect();
        if let Some(confs) = tagged {
            for (t, c) in result.tracks.iter_mut().zip(confs) {
                t.status = TrackStatus::AccurateTag;
                t.confidence = c;
            }
            result.verdict = Verdict::Accurate;
            result.reason = "AccurateRipResult tags".into();
            return result;
        }
    }

    let bounds: Arc<[u64]> = std::iter::once(0)
        .chain(sectors.iter().scan(0u64, |acc, &s| {
            *acc += s as u64 * SECTOR_FRAMES;
            Some(*acc)
        }))
        .collect();

    let lookup = db.lookup(&DiscId::from_track_sectors(&sectors));
    let mut pressings = match &lookup {
        Ok(Some(p)) if !p.is_empty() => Some(p.clone()),
        _ => None,
    };

    // Pass 1: offset 0, MD5 verification and frame-450 windows.
    let mut acc0 = OffsetAccum::new(0, bounds.clone());
    let mut windows = Frame450Windows::new(opts.offset_range, &bounds);
    let total = *bounds.last().unwrap() as f32;
    let passes = 1.5;
    let mut corrupted = vec![false; job.tracks.len()];
    for (k, track) in job.tracks.iter().enumerate() {
        let mut pos = bounds[k];
        let outcome = decode::decode(&track.path, true, |frames| {
            acc0.push(pos, frames);
            windows.push(pos, frames);
            pos += frames.len() as u64;
            progress(pos as f32 / total / passes);
        });
        let note = match outcome {
            Err(e) => Some(format!("decode error: {e}")),
            Ok(d) if d.verify_ok == Some(false) => Some("MD5 mismatch".into()),
            Ok(d) if d.frames != bounds[k + 1] - bounds[k] => Some(format!(
                "decoded {} frames, expected {}",
                d.frames,
                bounds[k + 1] - bounds[k]
            )),
            Ok(_) => None,
        };
        if let Some(note) = note {
            corrupted[k] = true;
            result.tracks[k].note = Some(note);
        }
    }

    let crc0 = acc0.finish();
    let n = crc0.len();
    let matched_at_zero = |p: &[Pressing]| {
        crc0.iter()
            .enumerate()
            .all(|(k, c)| find_match(k, c, Some(p)).confidence > 0)
    };

    // Candidate IDs. AccurateRip's ID depends on the real TOC, which the
    // audio files don't fully reveal: a data track (Enhanced or mixed-mode
    // CD) or a pregap before track 1 that wasn't ripped. If the audio-only ID
    // is unknown or doesn't fully match, ask CTDB for the real layouts and
    // try those IDs too.
    let mut sets: Vec<(Option<LayoutMatch>, db::Pressings)> = Vec::new();
    if let Some(p) = pressings.take() {
        sets.push((None, p));
    }
    if opts.use_ctdb && !sets.first().is_some_and(|(_, p)| matched_at_zero(p)) {
        sets.extend(
            find_layouts(db, &sectors)
                .into_iter()
                .map(|(m, p)| (Some(m), p)),
        );
    }

    // Pass 2: checksums at every pressing offset that some candidate's
    // frame-450 checksums point to, all in one more decode.
    let mut offsets: Vec<i64> = Vec::new();
    for (_, p) in sets.iter().filter(|(_, p)| !matched_at_zero(p)) {
        for o in candidate_offsets(p, &windows, opts.max_candidates) {
            if !offsets.contains(&o) {
                offsets.push(o);
            }
        }
    }
    let mut crcs: Vec<(i64, Vec<TrackCrc>)> = vec![(0, crc0)];
    if !offsets.is_empty() {
        let mut accs: Vec<OffsetAccum> = offsets
            .iter()
            .map(|&o| OffsetAccum::new(o, bounds.clone()))
            .collect();
        for (k, track) in job.tracks.iter().enumerate() {
            let mut pos = bounds[k];
            let _ = decode::decode(&track.path, false, |frames| {
                for a in accs.iter_mut() {
                    a.push(pos, frames);
                }
                pos += frames.len() as u64;
                progress((1.0 + pos as f32 / total * 0.5) / passes);
            });
        }
        crcs.extend(accs.iter().map(|a| (a.offset, a.finish())));
    }

    // Each track's best match over all offsets, for one candidate ID.
    let evaluate = |p: Option<&[Pressing]>| -> Vec<(i64, TrackCrc, Match)> {
        (0..n)
            .map(|k| {
                let (o0, c0) = &crcs[0];
                let mut best = (*o0, c0[k], find_match(k, &c0[k], p));
                for (o, c) in &crcs[1..] {
                    let m = find_match(k, &c[k], p);
                    if m.confidence > best.2.confidence {
                        best = (*o, c[k], m);
                    }
                }
                best
            })
            .collect()
    };

    // Keep the ID with the most matching tracks, then the most confidence;
    // ties go to the audio-only ID.
    let mut chosen = None;
    let mut chosen_score = None;
    for (layout, p) in sets {
        let eval = evaluate(Some(&p));
        let score = (
            eval.iter().filter(|b| b.2.confidence > 0).count(),
            eval.iter().map(|b| b.2.confidence).sum::<u32>(),
        );
        if chosen_score.is_none_or(|s| score > s) {
            chosen_score = Some(score);
            chosen = Some((layout, p, eval));
        }
    }
    let mut data_first = false;
    let mut real_layout = None;
    let best = match chosen {
        Some((layout, p, eval)) => {
            if let Some(m) = layout {
                real_layout = Some(m.layout);
                result.disc_id = Some(m.id.file_name());
                result.url = Some(m.id.url());
                result.data_track = Some(m.note);
                match m.layout {
                    TocLayout::DataFirst { .. } => data_first = true,
                    TocLayout::Pregap { start } => set_mb_id(job, &mut result, &sectors, start),
                    TocLayout::DataLast { .. } => {}
                }
            }
            pressings = Some(p);
            eval
        }
        None => evaluate(None),
    };
    result.pressings = pressings.as_ref().map_or(0, |p| p.len());

    for (k, (offset, crc, m)) in best.into_iter().enumerate() {
        let t = &mut result.tracks[k];
        t.v1 = Some(crc.v1);
        t.v2 = Some(crc.v2);
        t.offset = offset;
        t.db_crc = m.db_crc;
        t.confidence = m.confidence;
        t.status = if corrupted[k] {
            TrackStatus::Corrupted
        } else if pressings.is_none() || m.db_crc.is_none() {
            TrackStatus::NotInDb
        } else if m.confidence >= opts.min_confidence.max(1) {
            TrackStatus::Accurate
        } else if m.confidence > 0 {
            TrackStatus::LowConfidence
        } else {
            TrackStatus::Mismatch
        };
    }

    let count = |s: TrackStatus| result.tracks.iter().filter(|t| t.status == s).count();
    let n = result.tracks.len();
    // The last track of a data-first disc can't be verified, which is expected.
    let unverifiable = (data_first
        && result
            .tracks
            .last()
            .is_some_and(|t| t.status == TrackStatus::NotInDb)) as usize;
    (result.verdict, result.reason) = if count(TrackStatus::Corrupted) > 0 {
        (
            Verdict::Corrupted,
            format!("{} of {n} tracks corrupted", count(TrackStatus::Corrupted)),
        )
    } else if let (None, Err(e)) = (&pressings, &lookup) {
        (
            Verdict::CannotCheck,
            format!("AccurateRip lookup failed: {e}"),
        )
    } else if pressings.as_ref().is_none_or(|p| p.is_empty()) {
        (Verdict::CannotCheck, "Not in AccurateRip database".into())
    } else if count(TrackStatus::Mismatch) > 0 {
        (
            Verdict::RipErrors,
            format!(
                "{} of {n} tracks do not match",
                count(TrackStatus::Mismatch)
            ),
        )
    } else if count(TrackStatus::Accurate) + unverifiable == n {
        let mut notes = Vec::new();
        if let Some(o) = result.tracks.iter().map(|t| t.offset).find(|&o| o != 0) {
            notes.push(format!("pressing offset {o:+}"));
        }
        match (&result.data_track, data_first) {
            (Some(_), true) => notes.push("mixed-mode CD; last track not in AccurateRip".into()),
            (Some(n), false) if n.starts_with("Enhanced") => notes.push("Enhanced CD".into()),
            (Some(_), false) => notes.push("track 1 pregap".into()),
            (None, _) => {}
        }
        let reason = match notes.is_empty() {
            true => "Accurate".to_string(),
            false => format!("Accurate ({})", notes.join("; ")),
        };
        (Verdict::Accurate, reason)
    } else {
        let low = count(TrackStatus::LowConfidence);
        let missing = count(TrackStatus::NotInDb);
        (
            Verdict::CannotCheck,
            format!(
                "{low} tracks below confidence {}, {missing} not in database",
                opts.min_confidence
            ),
        )
    };
    if result.verdict == Verdict::RipErrors {
        result.diagnosis = sample_slip(&result.tracks);
    }
    let toc = db::ctdb_toc(
        &disc::audio_offsets(&sectors),
        sectors.iter().sum(),
        real_layout,
    );
    result.ctdb_toc = Some(toc.clone());
    if result.verdict == Verdict::RipErrors && opts.use_ctdb && result.diagnosis.is_none() {
        let note = match db::ctdb_repair_info(db, &toc) {
            Ok(Some((conf, parity))) => {
                result.ctdb_known = true;
                let data = if parity {
                    "with repair data"
                } else {
                    "no repair data"
                };
                format!("In CTDB (confidence {conf}), {data}")
            }
            Ok(None) => "Not in CTDB".into(),
            Err(e) => format!("CTDB lookup failed: {e}"),
        };
        result.ctdb_repair = Some(note);
    }
    if result.diagnosis.is_some() {
        result.reason.push_str("; sample slip, re-rip needed");
    }
    result.recheck = result.verdict != Verdict::Accurate;
    result
}

/// Whether cached results still hold under the current options.
pub fn cached_valid(albums: &[AlbumResult], opts: &Options) -> bool {
    // Results from the other mode answer a different question.
    albums.iter().all(|a| a.ctdb_first == opts.ctdb_first)
        && albums
            .iter()
            .flat_map(|a| &a.tracks)
            .all(|t| match t.status {
                TrackStatus::Accurate => t.confidence >= opts.min_confidence,
                TrackStatus::AccurateTag => opts.use_tags,
                _ => true,
            })
}

/// `--invert`: checks the album against CTDB's parity data and asks
/// AccurateRip only about albums that don't match it, reusing `ar` (a
/// [`verify`] result) if given. `dest` is where repaired copies go
/// (`--repair`). Also returns the CTDB check's outcome and description, if
/// it ran.
pub fn verify_ctdb_first(
    job: &AlbumJob,
    db: &Db,
    opts: &Options,
    dest: Option<&Path>,
    ar: Option<&AlbumResult>,
    progress: Progress<'_>,
) -> (AlbumResult, Option<(Outcome, String)>) {
    let mut result = blank_result(job);
    result.ctdb_first = true;
    if let Err(reason) = check_eligible(job) {
        result.reason = reason;
        return (result, None);
    }
    let sectors = identify(job, &mut result);
    let offsets = disc::audio_offsets(&sectors);
    let audio_end: u32 = sectors.iter().sum();
    let layout = match ctdb_layout(db, &offsets, audio_end) {
        Ok(Some(layout)) => layout,
        Ok(None) => {
            result.reason = "Not in CTDB".into();
            result.recheck = true;
            return (result, None);
        }
        Err(e) => {
            result.reason = format!("CTDB lookup failed: {e}");
            result.recheck = true;
            return (result, None);
        }
    };
    let toc = db::ctdb_toc(&offsets, audio_end, layout);
    let analysis = repair::analyze(job, &toc, db, progress);
    let Some((confidence, offset)) = analysis.as_ref().ok().and_then(|a| a.verified()) else {
        let (outcome, msg) = match &analysis {
            Ok(a) => repair::finish(job, a, dest),
            Err(e) => repair::failed(e),
        };
        // Not a clean CTDB match: let AccurateRip have its say.
        let mut ar = match ar {
            Some(a) => a.clone(),
            None => verify(job, db, opts, progress),
        };
        ar.ctdb_first = true;
        ar.ctdb_known = outcome != Outcome::Failed;
        ar.ctdb_toc = Some(toc);
        ar.repair_outcome = Some(outcome);
        ar.repair = Some(msg.clone());
        match ar.verdict {
            Verdict::Accurate => ar.reason.push_str("; differs from CTDB"),
            Verdict::RipErrors => {
                ar.reason = format!("{}; {}", ar.reason, outcome.verdict_note());
            }
            Verdict::Corrupted => {}
            Verdict::CannotCheck if outcome == Outcome::Failed => {}
            Verdict::CannotCheck => {
                ar.reason = format!(
                    "Doesn't match CTDB, {}; AccurateRip: {}",
                    outcome.verdict_note(),
                    ar.reason
                );
                ar.verdict = Verdict::RipErrors;
                ar.recheck = true;
            }
        }
        return (ar, Some((outcome, msg)));
    };

    if let Some(layout) = layout {
        let (id, note) = layout_id(&sectors, layout);
        result.disc_id = Some(id.file_name());
        result.url = Some(id.url());
        result.data_track = Some(note);
        if let TocLayout::Pregap { start } = layout {
            set_mb_id(job, &mut result, &sectors, start);
        }
    }
    for t in &mut result.tracks {
        t.status = TrackStatus::AccurateCtdb;
    }
    result.verdict = Verdict::Accurate;
    result.reason = format!("Accurate (matches CTDB, confidence {confidence}, offset {offset:+})");
    result.ctdb_known = true;
    result.ctdb_toc = Some(toc);
    result.repair_outcome = Some(Outcome::Verified);
    let msg = analysis.map(|a| a.summary()).unwrap_or_default();
    (result, Some((Outcome::Verified, msg)))
}

/// The layout under which CTDB knows this disc (`None` for audio only),
/// or `Ok(None)` if it doesn't know it.
fn ctdb_layout(db: &Db, offsets: &[u32], audio_end: u32) -> Result<Option<Option<TocLayout>>> {
    let plain = db::ctdb_toc(offsets, audio_end, None);
    if db::ctdb_repair_info(db, &plain)?.is_some() {
        return Ok(Some(None));
    }
    Ok(db::ctdb_layouts(db, offsets, audio_end)?
        .into_iter()
        .next()
        .map(Some))
}

/// Looks up the data track of an Enhanced CD in CTDB and returns the first
/// resulting AccurateRip ID that the database knows.
struct LayoutMatch {
    layout: TocLayout,
    id: DiscId,
    note: String,
}

/// Looks up other layouts of this disc (data tracks, an unripped pregap) in
/// CTDB and returns those AccurateRip knows, with their pressings.
fn find_layouts(db: &Db, sectors: &[u32]) -> Vec<(LayoutMatch, db::Pressings)> {
    let offsets = disc::audio_offsets(sectors);
    let audio_end: u32 = sectors.iter().sum();
    let Ok(layouts) = db::ctdb_layouts(db, &offsets, audio_end) else {
        return Vec::new();
    };
    layouts
        .into_iter()
        .take(5)
        .filter_map(|layout| {
            let (id, note) = layout_id(sectors, layout);
            let mut pressings = db.lookup(&id).ok().flatten().filter(|p| !p.is_empty())?;
            if matches!(layout, TocLayout::DataFirst { .. }) {
                pressings = Arc::new(shift_out_data_track(&pressings));
            }
            Some((LayoutMatch { layout, id, note }, pressings))
        })
        .collect()
}

/// The AccurateRip ID of our audio tracks laid out as `layout`, with a note
/// describing the layout.
fn layout_id(sectors: &[u32], layout: TocLayout) -> (DiscId, String) {
    match layout {
        TocLayout::DataLast { start, leadout } => (
            DiscId::with_data_track(sectors, start, leadout),
            format!(
                "Enhanced CD: data track at sector {start}, {} sectors long (from CTDB)",
                leadout - start
            ),
        ),
        TocLayout::DataFirst {
            audio_start,
            leadout,
        } => (
            DiscId::with_leading_data_track(sectors, audio_start, leadout),
            format!(
                "Mixed-mode CD: data track 1, {audio_start} sectors long (from CTDB); \
                         AccurateRip has no entry for the last track"
            ),
        ),
        TocLayout::Pregap { start } => (
            DiscId::with_pregap(sectors, start),
            format!("Track 1 starts at sector {start}; the pregap isn't in the rip (from CTDB)"),
        ),
    }
}

/// On discs that start with a data track, AccurateRip's entries are numbered
/// by disc track: entry 1 is the data track and the last audio track has
/// none. Re-index so entry `k` belongs to audio track `k`, with an empty
/// (unverifiable) entry for the last one.
fn shift_out_data_track(pressings: &[Pressing]) -> Vec<Pressing> {
    pressings
        .iter()
        .map(|p| Pressing {
            entries: p
                .entries
                .iter()
                .skip(1)
                .copied()
                .chain(std::iter::once(Entry {
                    confidence: 0,
                    crc: 0,
                    crc450: 0,
                }))
                .collect(),
        })
        .collect()
}

/// Accurate tracks before and after a run of bad tracks matching at
/// different offsets means the bad tracks gained or lost samples: the drive
/// lost sync. Parity can't repair that; the tracks need re-ripping.
fn sample_slip(tracks: &[TrackResult]) -> Option<String> {
    let accurate = |t: &TrackResult| t.status == TrackStatus::Accurate;
    let mut notes = Vec::new();
    let mut last_good: Option<&TrackResult> = None;
    let mut bad: Vec<u32> = Vec::new();
    for t in tracks {
        if accurate(t) {
            if let Some(prev) = last_good
                && prev.offset != t.offset
                && !bad.is_empty()
            {
                let list: Vec<String> = bad.iter().map(u32::to_string).collect();
                notes.push(format!(
                    "track{} {} shifted the audio by {} samples (track {} matches at offset {:+}, \
                     track {} at {:+}): a drive sync error that repair data can't fix; re-rip {}",
                    if bad.len() == 1 { "" } else { "s" },
                    list.join(", "),
                    (t.offset - prev.offset).abs(),
                    prev.number,
                    prev.offset,
                    t.number,
                    t.offset,
                    if bad.len() == 1 { "it" } else { "them" },
                ));
            }
            last_good = Some(t);
            bad.clear();
        } else if t.status == TrackStatus::Mismatch {
            bad.push(t.number);
        }
    }
    (!notes.is_empty()).then(|| format!("Sample slip: {}", notes.join("; ")))
}

/// Start of the reason for albums that aren't 44.1 kHz/16-bit stereo.
pub const NOT_CD_QUALITY: &str = "Not CD quality";

/// Checks that an album can be verified at all (lossless CD audio, complete).
fn check_eligible(job: &AlbumJob) -> Result<(), String> {
    if let Some(t) = job.tracks.iter().find(|t| t.problem.is_some()) {
        return Err(t.problem.clone().unwrap());
    }
    for t in &job.tracks {
        if t.sample_rate != Some(44100) || t.bits != Some(16) || t.channels != Some(2) {
            return Err(format!(
                "{NOT_CD_QUALITY} ({} Hz, {} bit, {} ch)",
                t.sample_rate.unwrap_or(0),
                t.bits.unwrap_or(0),
                t.channels.unwrap_or(0)
            ));
        }
        match t.frames {
            None => return Err("Unknown track length".into()),
            Some(f) if f % SECTOR_FRAMES != 0 => {
                return Err("Track length is not whole CD sectors".into());
            }
            Some(f) if f < 5 * SECTOR_FRAMES => return Err("Track too short".into()),
            _ => {}
        }
    }
    let n = job.tracks.len() as u32;
    let numbers: Vec<Option<u32>> = job.tracks.iter().map(|t| t.number).collect();
    if numbers
        .iter()
        .enumerate()
        .any(|(i, &num)| num != Some(i as u32 + 1))
    {
        return Err("Incomplete album (track numbers not 1..n)".into());
    }
    if let Some(total) = job.tracks.iter().find_map(|t| t.total)
        && total != n
    {
        return Err(format!("Incomplete album ({n} of {total} tracks)"));
    }
    if n > 99 {
        return Err("More than 99 tracks".into());
    }
    Ok(())
}

/// Parses dBpoweramp's `ACCURATERIPRESULT`, e.g.
/// `AccurateRip: Accurate (confidence 12)   [ABCD1234]`, returning the confidence.
pub fn tag_confidence(tag: &str) -> Option<u32> {
    let lower = tag.to_ascii_lowercase();
    if lower.contains("not accurate") || lower.contains("inaccurate") {
        return None;
    }
    let rest = &lower[lower.find("accurate (confidence ")? + "accurate (confidence ".len()..];
    rest.split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Match {
    pub confidence: u32,
    pub db_crc: Option<u32>,
}

fn find_match(k: usize, crc: &TrackCrc, pressings: Option<&[Pressing]>) -> Match {
    let mut confidence = 0;
    let mut matched = None;
    let mut top: Option<Entry> = None;
    for e in pressings
        .unwrap_or_default()
        .iter()
        .filter_map(|p| p.entries.get(k).copied())
    {
        if e.crc == 0 {
            continue;
        }
        if e.crc == crc.v1 || e.crc == crc.v2 {
            confidence += e.confidence as u32;
            matched = Some(e.crc);
        } else if top.is_none_or(|t| e.confidence > t.confidence) {
            top = Some(e);
        }
    }
    Match {
        confidence,
        db_crc: matched.or(top.map(|e| e.crc)),
    }
}

/// Offsets (other than 0) at which some track's frame-450 checksum matches the
/// database, most frequently matching first.
fn candidate_offsets(pressings: &[Pressing], w: &Frame450Windows, max: usize) -> Vec<i64> {
    let range = w.range();
    let mut hits = vec![0u32; 2 * range as usize + 1];
    let ntracks = pressings.first().map_or(0, |p| p.entries.len());
    for k in 0..ntracks {
        let targets: Vec<u32> = pressings
            .iter()
            .filter_map(|p| p.entries.get(k))
            .map(|e| e.crc450)
            .filter(|&c| c != 0)
            .collect();
        if targets.is_empty() {
            continue;
        }
        for (i, c) in w.crc450_all(k).iter().enumerate() {
            if targets.contains(c) {
                hits[i] += 1;
            }
        }
    }
    let mut found: Vec<(u32, i64)> = hits
        .iter()
        .enumerate()
        .filter(|&(i, &h)| h > 0 && i as i64 != range)
        .map(|(i, &h)| (h, i as i64 - range))
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.abs().cmp(&b.1.abs())));
    found.into_iter().take(max).map(|(_, o)| o).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(number: u32, offset: i64, status: TrackStatus) -> TrackResult {
        TrackResult {
            number,
            path: PathBuf::new(),
            v1: None,
            v2: None,
            db_crc: None,
            confidence: 0,
            offset,
            status,
            note: None,
        }
    }

    #[test]
    fn detects_sample_slip() {
        use TrackStatus::*;
        let slip = [
            track(1, 48, Accurate),
            track(2, 0, Mismatch),
            track(3, 42, Accurate),
        ];
        let d = sample_slip(&slip).unwrap();
        assert!(d.contains("track 2 shifted the audio by 6 samples"), "{d}");
        // Same offset on both sides: an ordinary ripping error, maybe repairable.
        let plain = [
            track(1, 42, Accurate),
            track(2, 0, Mismatch),
            track(3, 42, Accurate),
        ];
        assert_eq!(sample_slip(&plain), None);
    }

    #[test]
    fn parses_ar_tag() {
        assert_eq!(
            tag_confidence("AccurateRip: Accurate (confidence 12)   [ABCD1234]"),
            Some(12)
        );
        assert_eq!(
            tag_confidence("AccurateRip: Not Accurate (confidence 3)"),
            None
        );
        assert_eq!(tag_confidence("AccurateRip: Inaccurate"), None);
        assert_eq!(tag_confidence("Not Present"), None);
    }
}
