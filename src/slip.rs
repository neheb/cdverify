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
//!
//! A drive can also lose sync twice, with a stretch between the slips that
//! AccurateRip sees at yet another offset (by a frame-450 checksum). Then
//! both slip points are searched for together. A track ripped on its own at
//! a different offset from the rest of the disc looks just like that, with
//! the slips at its start and end.

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
    /// The drive lost sync twice: once before and once after frame 450 of a
    /// track in `first..=last`, which lines up in between.
    #[serde(default)]
    pub twice: Option<Twice>,
}

/// The stretch between two slips: frame 450 of track `track` (an index)
/// lines up at `shift` frames from the tracks before the first slip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Twice {
    pub track: usize,
    pub shift: i64,
}

impl Slip {
    fn valid(&self, tracks: usize) -> bool {
        let bounds = self.first > 0 && self.first <= self.last + 1 && self.last < tracks;
        bounds
            && match self.twice {
                None => self.shift != 0,
                Some(t) => {
                    (self.first..=self.last).contains(&t.track)
                        && t.shift != 0
                        && t.shift != self.shift
                }
            }
    }

    /// The tracks to re-rip: those that don't match, or both sides of a
    /// slip between tracks.
    fn bad(&self) -> Vec<usize> {
        match self.first > self.last {
            true => vec![self.last, self.first],
            false => (self.first..=self.last).collect(),
        }
    }
}

/// Columns sampled to find the slip point, in frames (two words each).
const SAMPLE_FRAMES: usize = 64;

/// Sampled columns that must match CTDB's to accept a slip point.
const MIN_VOTES: u32 = SAMPLE_FRAMES as u32 / 4;

/// Most column lookups spent searching for two slip points (about a
/// minute's work): beyond it, the stretches they may be in are too long.
const MAX_TWICE_WORK: u64 = 4_000_000_000;

/// The album realigned at frames `at[0] <= at[1]`: its frame `i` is the
/// rip's frame `i + shift[0]` before `at[0]`, `i + shift[1]` up to `at[1]`
/// and `i + shift[2]` from `at[1]` on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Splice {
    at: [u64; 2],
    shift: [i64; 3],
}

impl Splice {
    /// Realigned at a single slip point.
    #[cfg(test)]
    fn once(at: u64, head: i64, tail: i64) -> Splice {
        Splice {
            at: [at, at],
            shift: [head, tail, tail],
        }
    }

    /// The rip's frame that the realigned album's frame `i` comes from.
    fn source(&self, i: u64) -> i64 {
        let piece = (i >= self.at[0]) as usize + (i >= self.at[1]) as usize;
        i as i64 + self.shift[piece]
    }

    /// The tracks (indices) it changes: those that a piece shifted by other
    /// than 0 overlaps.
    fn moved(&self, bounds: &[u64]) -> Vec<usize> {
        let total = bounds[bounds.len() - 1];
        let pieces = [
            (0, self.at[0], self.shift[0]),
            (self.at[0], self.at[1], self.shift[1]),
            (self.at[1], total, self.shift[2]),
        ];
        (0..bounds.len() - 1)
            .filter(|&k| {
                pieces
                    .iter()
                    .any(|&(a, b, s)| s != 0 && a < b && a < bounds[k + 1] && bounds[k] < b)
            })
            .collect()
    }
}

/// The shifts of the pieces before, between and after the slip points (see
/// [`Splice`]). The side with fewer tracks outside the slips is moved, the
/// tail if it's a tie; two slips that cancel out move only what's between
/// them.
fn shifts(slip: Slip, tracks: usize) -> [i64; 3] {
    let before = slip.first;
    let after = tracks - 1 - slip.last;
    let mid = slip.twice.map_or(slip.shift, |t| t.shift);
    let s = [0, mid, slip.shift];
    match slip.shift == 0 || after <= before {
        true => s,
        false => s.map(|x| x - slip.shift),
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
        // Later frames come from no earlier than this (the pieces still to
        // come shift by no less than the least shift).
        let least = self.splice.shift.iter().copied().min().unwrap_or(0);
        let keep = (self.next as i64 + least).max(0) as u64;
        let drop = keep.saturating_sub(self.start).min(self.buf.len() as u64);
        self.buf.drain(..drop as usize);
        self.start += drop;
    }
}

/// The words of the sampled columns, for the album realigned anywhere: for
/// column `j` (word column `cols[j]`) and row `r` (stride `r + 1`), word
/// `[j * rows + r]` of `words[s]` if the realigned album takes it from a
/// piece shifted by the `s`-th of the shifts gathered.
struct Samples {
    cols: Vec<usize>,
    rows: usize,
    words: Vec<Vec<u16>>,
}

impl Samples {
    /// The frame that row `r` of column `j` is in.
    fn frame(&self, geo: Geometry, j: usize, r: usize) -> u64 {
        ((geo.stride * (r + 1) + self.cols[j]) / 2) as u64
    }

    /// The syndromes of every sampled column, with row `r` of column `j`
    /// taken from `words[pick(frame)]`.
    fn syndromes(&self, geo: Geometry, npar: usize, pick: impl Fn(u64) -> usize) -> Vec<u16> {
        let mut syn = vec![0u16; self.cols.len() * npar];
        for (j, s) in syn.chunks_exact_mut(npar).enumerate() {
            let words =
                (0..self.rows).map(|r| self.words[pick(self.frame(geo, j, r))][j * self.rows + r]);
            parity::column_syndrome(s, words);
        }
        syn
    }
}

/// Decodes the album, keeping the sampled columns' words for each of
/// `shifts`.
fn gather(
    job: &AlbumJob,
    geo: Geometry,
    shifts: &[i64],
    progress: &mut dyn FnMut(f32),
) -> Result<Samples> {
    let total = (geo.words / 2) as u64;
    let rows = geo.stridecount;
    let ns = shifts.len();
    // Frame columns spread over the stride; both words of each.
    let frame_cols: Vec<usize> = (0..SAMPLE_FRAMES)
        .map(|k| k * geo.stride / 2 / SAMPLE_FRAMES)
        .collect();
    let cols: Vec<usize> = frame_cols
        .iter()
        .flat_map(|&f| [2 * f, 2 * f + 1])
        .collect();
    // The rip's frames wanted, with where each goes: (frame, slot), slot
    // `(k * rows + r) * ns + s` for frame column `k` and shift `s`.
    let mut wants: Vec<(u64, usize)> = Vec::with_capacity(SAMPLE_FRAMES * rows * ns);
    for (k, &fc) in frame_cols.iter().enumerate() {
        for r in 0..rows {
            let i = (geo.stride / 2 * (r + 1) + fc) as i64;
            for (s, shift) in shifts.iter().enumerate() {
                if let Ok(src) = u64::try_from(i + shift)
                    && src < total
                {
                    wants.push((src, (k * rows + r) * ns + s));
                }
            }
        }
    }
    wants.sort_unstable();
    let mut frames_at = vec![0u32; SAMPLE_FRAMES * rows * ns];
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
    let mut words = vec![Vec::with_capacity(SAMPLE_FRAMES * 2 * rows); ns];
    for k in 0..SAMPLE_FRAMES {
        for half in 0..2 {
            for r in 0..rows {
                for (s, w) in words.iter_mut().enumerate() {
                    w.push((frames_at[(k * rows + r) * ns + s] >> (16 * half)) as u16);
                }
            }
        }
    }
    Ok(Samples { cols, rows, words })
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

/// CTDB's columns by syndrome, given an entry's full syndrome
/// (`[stride][npar]`); one shared by several columns says nothing
/// (`usize::MAX`).
fn ctdb_columns(db_syn: &[u16], npar: usize) -> HashMap<[u16; MAX_NPAR], usize> {
    let mut columns: HashMap<[u16; MAX_NPAR], usize> = HashMap::new();
    for (c, syn) in db_syn.chunks_exact(npar).enumerate() {
        if syn.iter().any(|&s| s != 0) {
            columns
                .entry(key(syn))
                .and_modify(|v| *v = usize::MAX)
                .or_insert(c);
        }
    }
    columns
}

/// The column shift at which column `j`, with syndrome `syn`, matches one
/// of CTDB's, if it does.
fn column_match(
    columns: &HashMap<[u16; MAX_NPAR], usize>,
    samples: &Samples,
    geo: Geometry,
    j: usize,
    syn: &[u16],
) -> Option<usize> {
    columns
        .get(&key(syn))
        .filter(|&&c| c != usize::MAX)
        .map(|&c| (c + geo.stride - samples.cols[j]) % geo.stride)
}

/// Candidate slip points `stride / 4` frames apart in `from..=to`, both
/// ends included: one is within `stride / 8` frames of the slip point, so
/// that at most a quarter of the columns has a misaligned word there.
fn candidates(geo: Geometry, (from, to): (u64, u64)) -> impl Iterator<Item = u64> {
    let step = (geo.stride / 4).max(1) as u64;
    let n = (to - from).div_ceil(step);
    (0..=n).map(move |i| (from + i * step).min(to))
}

/// Finds the slip point in frames `region` (words from `words[0]` before
/// it, `words[1]` after), given an entry's full syndrome: the candidate
/// point at which the most sampled columns match CTDB's columns at one
/// offset.
fn locate(
    samples: &Samples,
    geo: Geometry,
    region: (u64, u64),
    db_syn: &[u16],
    npar: usize,
) -> Option<u64> {
    let columns = ctdb_columns(db_syn, npar);
    let rows = samples.rows;
    let (head, tail) = (&samples.words[0], &samples.words[1]);
    let mut syn = samples.syndromes(geo, npar, |f| (f >= region.0) as usize);
    let mut best: Option<(u32, u64)> = None;
    let mut votes: HashMap<usize, u32> = HashMap::new();
    let mut points = candidates(geo, region).peekable();
    while let Some(at) = points.next() {
        votes.clear();
        for (j, s) in syn.chunks_exact(npar).enumerate() {
            if let Some(delta) = column_match(&columns, samples, geo, j, s) {
                *votes.entry(delta).or_default() += 1;
            }
        }
        let score = votes.values().copied().max().unwrap_or(0);
        if best.is_none_or(|(s, _)| score > s) {
            best = Some((score, at));
        }
        let Some(&next) = points.peek() else { break };
        // Rows in frames `at..next` now come from before the slip point.
        for j in 0..samples.cols.len() {
            let col = samples.cols[j] as u64;
            let stride = geo.stride as u64;
            // The first row at or after frame `at`.
            let first = (2 * at).saturating_sub(stride + col).div_ceil(stride) as usize;
            for r in first..rows {
                if samples.frame(geo, j, r) >= next {
                    break;
                }
                let i = j * rows + r;
                let diff = head[i] ^ tail[i];
                if diff != 0 {
                    parity::change_row(&mut syn[j * npar..(j + 1) * npar], rows, r, diff);
                }
            }
        }
    }
    best.filter(|&(score, _)| score >= MIN_VOTES)
        .map(|(_, at)| at)
}

/// Finds two slip points together: the first in frames `first`, the second
/// in `second` (which comes after), with words from `words[0]` before the
/// first, `words[1]` between and `words[2]` after the second. As
/// [`locate`], but over every pair of candidate points. Syndromes are
/// linear, so a column's is its syndrome with both points at the start of
/// their stretches, plus a change for the first point that depends only on
/// it, plus one for the second, kept up to date while sweeping it. `None`
/// if no pair matches, or the stretches are too long to search.
fn locate_twice(
    samples: &Samples,
    geo: Geometry,
    first: (u64, u64),
    second: (u64, u64),
    db_syn: &[u16],
    npar: usize,
) -> Option<(u64, u64)> {
    let columns = ctdb_columns(db_syn, npar);
    let rows = samples.rows;
    let ncols = samples.cols.len();
    let [w0, w1, w2] = &samples.words[..] else {
        return None;
    };
    let p1s: Vec<u64> = candidates(geo, first).collect();
    let n2 = candidates(geo, second).count() as u64;
    if p1s.len() as u64 * n2 * ncols as u64 > MAX_TWICE_WORK {
        return None;
    }
    let mut base = samples.syndromes(geo, npar, |f| match f {
        f if f < first.0 => 0,
        f if f < second.0 => 1,
        _ => 2,
    });
    // The change for the first point at `p1s[b]`, column `j`: at
    // `[(j * nb + b) * npar..]`.
    let nb = p1s.len();
    let mut ones = vec![0u16; ncols * nb * npar];
    let mut acc = vec![0u16; npar];
    for j in 0..ncols {
        acc.fill(0);
        let mut r = 0;
        for (b, &p1) in p1s.iter().enumerate() {
            while r < rows && samples.frame(geo, j, r) < p1 {
                let i = j * rows + r;
                let diff = w0[i] ^ w1[i];
                if samples.frame(geo, j, r) >= first.0 && diff != 0 {
                    parity::change_row(&mut acc, rows, r, diff);
                }
                r += 1;
            }
            ones[(j * nb + b) * npar..(j * nb + b + 1) * npar].copy_from_slice(&acc);
        }
    }
    // Each column's first row at or after the second point.
    let mut row2: Vec<usize> = (0..ncols)
        .map(|j| {
            (0..rows)
                .find(|&r| samples.frame(geo, j, r) >= second.0)
                .unwrap_or(rows)
        })
        .collect();
    let mut best: Option<(u32, u64, u64)> = None;
    let mut votes: HashMap<(usize, usize), u32> = HashMap::new();
    let mut syn = vec![0u16; npar];
    let mut points = candidates(geo, second).peekable();
    while let Some(p2) = points.next() {
        votes.clear();
        for j in 0..ncols {
            let bj = &base[j * npar..(j + 1) * npar];
            for b in 0..nb {
                let oj = &ones[(j * nb + b) * npar..(j * nb + b + 1) * npar];
                for (s, (x, y)) in syn.iter_mut().zip(bj.iter().zip(oj)) {
                    *s = x ^ y;
                }
                if let Some(delta) = column_match(&columns, samples, geo, j, &syn) {
                    *votes.entry((b, delta)).or_default() += 1;
                }
            }
        }
        if let Some((&(b, _), &score)) = votes.iter().max_by_key(|&(_, &v)| v)
            && best.is_none_or(|(s, _, _)| score > s)
        {
            best = Some((score, p1s[b], p2));
        }
        let Some(&next) = points.peek() else { break };
        // Rows in frames `p2..next` now come from between the points.
        for (j, r) in row2.iter_mut().enumerate() {
            while *r < rows && samples.frame(geo, j, *r) < next {
                let i = j * rows + *r;
                let diff = w1[i] ^ w2[i];
                if diff != 0 {
                    parity::change_row(&mut base[j * npar..(j + 1) * npar], rows, *r, diff);
                }
                *r += 1;
            }
        }
    }
    best.filter(|&(score, _, _)| score >= MIN_VOTES)
        .map(|(_, p1, p2)| (p1, p2))
}

/// Where in the album frame `at` is, for a message.
fn place(job: &AlbumJob, bounds: &[u64], at: u64) -> String {
    let n = job.tracks.len();
    let number = |k: usize| job.tracks[k].number.unwrap_or(k as u32 + 1);
    let k = bounds.partition_point(|&b| b <= at).clamp(1, n) - 1;
    let secs = (at - bounds[k]) / (SECTOR_FRAMES * 75);
    match (k, secs) {
        (1.., 0) if at == bounds[k] => {
            format!("between tracks {} and {}", number(k - 1), number(k))
        }
        _ => format!("in track {} at {}:{:02}", number(k), secs / 60, secs % 60),
    }
}

/// What a slip of `shift` frames did.
fn what(shift: i64) -> &'static str {
    match shift < 0 {
        true => "dropped",
        false => "repeated",
    }
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
    if !slip.valid(n) {
        bail!("no sample slip to repair");
    }
    let bounds = repair::track_bounds(job)?;
    let total = bounds[n];
    let number = |k: usize| job.tracks[k].number.unwrap_or(k as u32 + 1);
    let bad = slip.bad();
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
    let shift = shifts(slip, n);
    let region = (bounds[slip.first], bounds[slip.last + 1]);
    // With two slips, frame 450 of the track that lines up between them
    // separates them.
    let twice = slip.twice.map(|t| {
        let at = (bounds[t.track] + 450 * SECTOR_FRAMES).clamp(region.0, region.1);
        let after = (at + SECTOR_FRAMES).min(region.1);
        ((region.0, at), (after, region.1))
    });

    let gathered: &[i64] = match twice {
        Some(_) => &shift,
        None => &[shift[0], shift[2]],
    };
    let samples = gather(job, geo, gathered, &mut |f| progress(f / 3.0))?;
    let mut at = None;
    for e in &entries {
        let Some(url) = &e.parity_url else { continue };
        let npar = repair::entry_npar(e);
        let bytes = db::fetch(db, url).context("downloading CTDB parity")?;
        let Some(syn) = parity::bytes_to_syndrome(geo.stride, npar, &bytes) else {
            continue;
        };
        at = match twice {
            Some((first, second)) => {
                locate_twice(&samples, geo, first, second, &syn, npar).map(|(p1, p2)| [p1, p2])
            }
            None => locate(&samples, geo, region, &syn, npar).map(|p| [p, p]),
        };
        if at.is_some() {
            break;
        }
    }
    let Some(at) = at else {
        return not("couldn't find where the drive lost sync in CTDB's repair data");
    };
    let found = Splice { at, shift };
    // Two slips that cancel out near a track's start and end: the track was
    // ripped on its own at a different offset, so they're exactly there.
    // The located points are only within `stride / 8` frames.
    let near = |a: u64, b: u64| a.abs_diff(b) <= (geo.stride / 4) as u64;
    let own_offset = (twice.is_some() && shift[0] == shift[2])
        .then(|| (0..n).find(|&k| near(at[0], bounds[k]) && near(at[1], bounds[k + 1])))
        .flatten();
    let mut tries = vec![found];
    if let Some(k) = own_offset {
        tries.insert(
            0,
            Splice {
                at: [bounds[k], bounds[k + 1]],
                shift,
            },
        );
    }

    let mut result = None;
    let mut last_why = String::new();
    for (i, &splice) in tries.iter().enumerate() {
        // The stride `locate` searched: other entries' columns differ.
        let mut pass = repair::Pass::with_stride(job, toc, db, Some(stride_frames))?;
        let mut spliced = Spliced::new(splice, total);
        let part = |f: f32| (1.0 + (i as f32 + f) / tries.len() as f32) / 3.0;
        let notes = decode::decode_album(&job.tracks, false, |pos, frames| {
            spliced.push(frames, &mut |f| pass.push(f));
            progress(part((pos + frames.len() as u64) as f32 / total as f32));
        });
        spliced.finish(&mut |f| pass.push(f));
        let analysis = pass.finish(&notes, db)?;
        if analysis.fix_plan().is_some() {
            result = Some((splice, analysis));
            break;
        }
        last_why = analysis.summary();
    }
    let Some((splice, analysis)) = result else {
        let why = last_why.trim_start_matches("Not repairable: ");
        return not(&format!("realigned at the slip, {why}"));
    };
    let Some((confidence, _, fixes)) = analysis.fix_plan() else {
        unreachable!("checked above")
    };

    // What the drive did, and where.
    let [s0, s1, s2] = splice.shift;
    let did = match (slip.twice, own_offset) {
        (Some(_), Some(k)) if splice.at == [bounds[k], bounds[k + 1]] => format!(
            "track {} was ripped {} samples off the rest of the disc",
            number(k),
            (s1 - s0).abs()
        ),
        (Some(_), _) => format!(
            "the drive {} {} samples {} and {} {} samples {}",
            what(s1 - s0),
            (s1 - s0).abs(),
            place(job, &bounds, splice.at[0]),
            what(s2 - s1),
            (s2 - s1).abs(),
            place(job, &bounds, splice.at[1]),
        ),
        (None, _) => format!(
            "the drive {} {} samples {}",
            what(slip.shift),
            slip.shift.abs(),
            place(job, &bounds, splice.at[0])
        ),
    };
    // The moved tracks, and any others the corrections change.
    let mut tracks = splice.moved(&bounds);
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
        "Repairable: {did}; realigning track{} {list} and correcting {} samples matches CTDB \
         (confidence {confidence})",
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
    use crate::parity::ParityEncoder;

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
            let s = splice_all(&rip, Splice::once(20, 0, -6), chunk);
            assert_eq!(s.len(), 50);
            assert_eq!(&s[..20], &rip[..20]);
            assert_eq!(&s[20..], &rip[14..44]);
            // Repeated, moving the head: frames before 20 come 6 later, and
            // the first 6 aren't in the rip.
            let s = splice_all(&rip, Splice::once(20, 6, 0), chunk);
            assert_eq!(&s[..14], &rip[6..20]);
            assert_eq!(&s[14..20], &rip[20..26]);
            assert_eq!(&s[20..], &rip[20..]);
            let s = splice_all(&rip, Splice::once(20, -6, 0), chunk);
            assert_eq!(&s[..6], &[0; 6]);
            assert_eq!(&s[6..20], &rip[..14]);
            assert_eq!(&s[20..], &rip[20..]);
            // Frames 10..30 moved 4 later.
            let s = splice_all(
                &rip,
                Splice {
                    at: [10, 30],
                    shift: [0, 4, 0],
                },
                chunk,
            );
            assert_eq!(&s[..10], &rip[..10]);
            assert_eq!(&s[10..30], &rip[14..34]);
            assert_eq!(&s[30..], &rip[30..]);
        }
    }

    /// What `gather` keeps, from memory.
    fn samples(rip: &[u32], geo: Geometry, shifts: &[i64]) -> Samples {
        let rows = geo.stridecount;
        let frame_cols: Vec<usize> = (0..SAMPLE_FRAMES)
            .map(|k| k * geo.stride / 2 / SAMPLE_FRAMES)
            .collect();
        let cols: Vec<usize> = frame_cols
            .iter()
            .flat_map(|&f| [2 * f, 2 * f + 1])
            .collect();
        let at = |i: i64| usize::try_from(i).ok().and_then(|i| rip.get(i)).copied();
        let mut words = vec![Vec::new(); shifts.len()];
        for &fc in &frame_cols {
            for half in 0..2 {
                for r in 0..rows {
                    let i = (geo.stride / 2 * (r + 1) + fc) as i64;
                    for (w, s) in words.iter_mut().zip(shifts) {
                        w.push((at(i + s).unwrap_or(0) >> (16 * half)) as u16);
                    }
                }
            }
        }
        Samples { cols, rows, words }
    }

    /// Checks that the parity corrects `rip` realigned by `splice` into
    /// `truth`.
    fn repairs(
        truth: &[u32],
        rip: &[u32],
        geo: Geometry,
        db_syn: &[u16],
        npar: usize,
        splice: Splice,
    ) {
        let s = splice_all(rip, splice, 1000);
        let mut enc = ParityEncoder::new(geo);
        enc.push(&s);
        let fixes = parity::compute_fixes(&enc, db_syn, npar, 0).expect("repairable");
        let mut fixed = s.clone();
        for f in &fixes {
            fixed[f.word / 2] ^= (f.diff as u32) << (f.word % 2 * 16);
        }
        let (a, b) = parity::crc_window(geo, 0);
        assert_eq!(fixed[a as usize..b as usize], truth[a as usize..b as usize]);
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
        let stride = 512; // words
        let npar = 8;
        let len = stride / 2 * 60;
        for (slip, shift) in [(9000, -6), (9000, 6), (5003, -40)] {
            let (truth, rip) = slipped(len, slip, shift);
            let geo = Geometry::new(len as u64, stride);
            let mut enc = ParityEncoder::new(geo);
            enc.push(&truth);
            let db_syn = enc.syndrome(npar, stride, 0);
            let s = samples(&rip, geo, &[0, shift]);
            let found = locate(&s, geo, (4000, 12000), &db_syn, npar).expect("found");
            assert!(
                found.abs_diff(slip as u64) <= stride as u64 / 4,
                "slip {slip} shift {shift}: found {found}"
            );
            // Realigned there, the rip is the truth but for a few frames
            // around the slip, which the parity corrects.
            repairs(
                &truth,
                &rip,
                geo,
                &db_syn,
                npar,
                Splice::once(found, 0, shift),
            );
        }
    }

    #[test]
    fn locates_two_slips() {
        let stride = 512;
        let npar = 16;
        let len = stride / 2 * 400;
        let disc = noise(len + 400, 7);
        let truth = &disc[200..200 + len];
        // The rip's frames `p1..p2` hold the truth's from `mid` frames on,
        // those after `tail` frames on.
        for (p1, p2, mid, tail) in [
            (30000, 60000, 7, 0),
            (30000, 60000, -12, 0),
            (30500, 71234, 7, 57),
            (30000, 60000, 40, -9),
        ] {
            let rip: Vec<u32> = (0..len)
                .map(|i| {
                    let s = match i {
                        i if i < p1 => 0,
                        i if i < p2 => mid,
                        _ => tail,
                    };
                    disc[(200 + i as i64 - s) as usize]
                })
                .collect();
            let geo = Geometry::new(len as u64, stride);
            let mut enc = ParityEncoder::new(geo);
            enc.push(truth);
            let db_syn = enc.syndrome(npar, stride, 0);
            let shift = [0, mid, tail];
            let s = samples(&rip, geo, &shift);
            let (f1, f2) = locate_twice(&s, geo, (28000, 33000), (33588, 90000), &db_syn, npar)
                .expect("found");
            let tol = stride as u64 / 4;
            assert!(
                f1.abs_diff(p1 as u64) <= tol && f2.abs_diff(p2 as u64) <= tol,
                "{p1}..{p2}: found {f1}..{f2}"
            );
            repairs(
                truth,
                &rip,
                geo,
                &db_syn,
                npar,
                Splice {
                    at: [f1, f2],
                    shift,
                },
            );
        }
    }

    #[test]
    fn moves_the_shorter_side() {
        let slip = Slip {
            first: 1,
            last: 1,
            shift: -6,
            twice: None,
        };
        assert_eq!(shifts(slip, 10), [6, 0, 0]);
        assert_eq!(shifts(slip, 3), [0, -6, -6]);
        let late = Slip {
            first: 7,
            last: 7,
            shift: -6,
            twice: None,
        };
        assert_eq!(shifts(late, 10), [0, -6, -6]);
        // Two slips that cancel out move only what's between them.
        let own = Slip {
            first: 1,
            last: 1,
            shift: 0,
            twice: Some(Twice {
                track: 1,
                shift: 48,
            }),
        };
        assert_eq!(shifts(own, 10), [0, 48, 0]);
        assert!(own.valid(10));
        let twice = Slip {
            first: 1,
            last: 1,
            shift: -6,
            twice: Some(Twice { track: 1, shift: 4 }),
        };
        assert_eq!(shifts(twice, 10), [6, 10, 0]);
        // The middle must line up somewhere else.
        let no = Slip {
            twice: Some(Twice {
                track: 1,
                shift: -6,
            }),
            ..twice
        };
        assert!(!no.valid(10));
    }

    #[test]
    fn finds_moved_tracks() {
        let bounds = [0, 100, 200, 300, 400];
        let splice = |at, shift| Splice { at, shift };
        assert_eq!(splice([150, 150], [0, 6, 6]).moved(&bounds), vec![1, 2, 3]);
        assert_eq!(splice([150, 150], [-6, 0, 0]).moved(&bounds), vec![0, 1]);
        assert_eq!(splice([200, 200], [0, 6, 6]).moved(&bounds), vec![2, 3]);
        assert_eq!(splice([100, 200], [0, 48, 0]).moved(&bounds), vec![1]);
        assert_eq!(splice([150, 250], [0, 48, 0]).moved(&bounds), vec![1, 2]);
    }
}
