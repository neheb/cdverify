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

/// The album realigned in pieces: from frame `pieces[j].0` up to the next
/// piece, its frame `i` is the rip's frame `i + pieces[j].1`. Frames before
/// the first piece aren't moved. Pieces are in order.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Splice {
    pieces: Vec<(u64, i64)>,
}

impl Splice {
    /// Realigned at frames `at[0] <= at[1]`: by `shift[0]` before `at[0]`,
    /// `shift[1]` up to `at[1]` and `shift[2]` from `at[1]` on.
    fn new(at: [u64; 2], shift: [i64; 3]) -> Splice {
        Splice {
            pieces: vec![(0, shift[0]), (at[0], shift[1]), (at[1], shift[2])],
        }
    }

    /// Every frame moved by `shift`.
    fn by(shift: i64) -> Splice {
        Splice {
            pieces: vec![(0, shift)],
        }
    }

    /// Realigned at a single slip point.
    #[cfg(test)]
    fn once(at: u64, head: i64, tail: i64) -> Splice {
        Splice::new([at, at], [head, tail, tail])
    }

    /// How far frame `i` is moved.
    fn shift(&self, i: u64) -> i64 {
        let j = self.pieces.partition_point(|p| p.0 <= i);
        j.checked_sub(1).map_or(0, |j| self.pieces[j].1)
    }

    /// The rip's frame that the realigned album's frame `i` comes from.
    fn source(&self, i: u64) -> i64 {
        i as i64 + self.shift(i)
    }

    /// The least any frame is moved.
    fn least(&self) -> i64 {
        let unmoved = self.pieces.first().is_none_or(|p| p.0 > 0);
        let least = self.pieces.iter().map(|p| p.1).min().unwrap_or(0);
        if unmoved { least.min(0) } else { least }
    }

    /// The tracks (indices) it changes: those that a piece shifted by other
    /// than 0 overlaps.
    fn moved(&self, bounds: &[u64]) -> Vec<usize> {
        let total = bounds[bounds.len() - 1];
        let ranges: Vec<(u64, u64, i64)> = self
            .pieces
            .iter()
            .enumerate()
            .map(|(j, &(a, s))| (a, self.pieces.get(j + 1).map_or(total, |p| p.0), s))
            .collect();
        (0..bounds.len() - 1)
            .filter(|&k| {
                ranges
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
        let least = self.splice.least();
        let keep = (self.next as i64 + least).max(0) as u64;
        let drop = keep.saturating_sub(self.start).min(self.buf.len() as u64);
        self.buf.drain(..drop as usize);
        self.start += drop;
    }
}

/// The words of the sampled columns, for the album realigned anywhere: for
/// column `j` (word column `cols[j]`) and row `r` (stride `r + 1`), word
/// `[j * rows + r]` of `words[s]` in the album realigned by the `s`-th
/// splice gathered.
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

/// Decodes the album, keeping the sampled columns' words for the album
/// realigned by each of `splices`.
fn gather(
    job: &AlbumJob,
    geo: Geometry,
    splices: &[Splice],
    progress: &mut dyn FnMut(f32),
) -> Result<Samples> {
    let total = (geo.words / 2) as u64;
    let rows = geo.stridecount;
    let ns = splices.len();
    // Frame columns spread over the stride; both words of each.
    let frame_cols: Vec<usize> = (0..SAMPLE_FRAMES)
        .map(|k| k * geo.stride / 2 / SAMPLE_FRAMES)
        .collect();
    let cols: Vec<usize> = frame_cols
        .iter()
        .flat_map(|&f| [2 * f, 2 * f + 1])
        .collect();
    // The rip's frames wanted, with where each goes: (frame, slot), slot
    // `(k * rows + r) * ns + s` for frame column `k` and splice `s`.
    let mut wants: Vec<(u64, usize)> = Vec::with_capacity(SAMPLE_FRAMES * rows * ns);
    for (k, &fc) in frame_cols.iter().enumerate() {
        for r in 0..rows {
            let i = (geo.stride / 2 * (r + 1) + fc) as u64;
            for (s, splice) in splices.iter().enumerate() {
                if let Ok(src) = u64::try_from(splice.source(i))
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

/// Finds the slip point in frames `region` (words from `words[h]` before
/// it, `words[t]` after), given an entry's full syndrome: the candidate
/// point at which the most sampled columns match CTDB's columns at one
/// offset, with how many do.
fn locate_scored(
    samples: &Samples,
    (h, t): (usize, usize),
    geo: Geometry,
    region: (u64, u64),
    db_syn: &[u16],
    npar: usize,
) -> Option<(u32, u64)> {
    let columns = ctdb_columns(db_syn, npar);
    let rows = samples.rows;
    let (head, tail) = (&samples.words[h], &samples.words[t]);
    let mut syn = samples.syndromes(geo, npar, |f| if f < region.0 { h } else { t });
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
}

/// [`locate_scored`], without the score.
fn locate(
    samples: &Samples,
    words: (usize, usize),
    geo: Geometry,
    region: (u64, u64),
    db_syn: &[u16],
    npar: usize,
) -> Option<u64> {
    locate_scored(samples, words, geo, region, db_syn, npar).map(|(_, at)| at)
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

    let gathered: Vec<Splice> = match twice {
        Some(_) => shift.to_vec(),
        None => vec![shift[0], shift[2]],
    }
    .into_iter()
    .map(Splice::by)
    .collect();
    let samples = gather(job, geo, &gathered, &mut |f| progress(f / 3.0))?;
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
            None => locate(&samples, (0, 1), geo, region, &syn, npar).map(|p| [p, p]),
        };
        if at.is_some() {
            break;
        }
    }
    let Some(at) = at else {
        return not("couldn't find where the drive lost sync in CTDB's repair data");
    };
    // Two slips that cancel out near a track's start and end: the track was
    // ripped on its own at a different offset, so they're exactly there.
    // The located points are only within `stride / 8` frames.
    let near = |a: u64, b: u64| a.abs_diff(b) <= (geo.stride / 4) as u64;
    let own_offset = (twice.is_some() && shift[0] == shift[2])
        .then(|| (0..n).find(|&k| near(at[0], bounds[k]) && near(at[1], bounds[k + 1])))
        .flatten();
    let mut tries = vec![at];
    if let Some(k) = own_offset {
        tries.insert(0, [bounds[k], bounds[k + 1]]);
    }

    let mut result = None;
    let mut last_why = String::new();
    for (i, &at) in tries.iter().enumerate() {
        let splice = Splice::new(at, shift);
        let part = |f: f32| (1.0 + (i as f32 + f) / tries.len() as f32) / 3.0;
        let checked = check(job, toc, db, stride_frames, &splice, &mut |f| {
            progress(part(f))
        })?;
        if repairs(&checked.analysis).is_some() {
            result = Some((at, splice, checked));
            break;
        }
        last_why = checked.analysis.summary();
    }
    let Some((at, splice, checked)) = result else {
        let why = last_why.trim_start_matches("Not repairable: ");
        return not(&format!("realigned at the slip, {why}"));
    };

    // What the drive did, and where.
    let [s0, s1, s2] = shift;
    let did = match (slip.twice, own_offset) {
        (Some(_), Some(k)) if at == [bounds[k], bounds[k + 1]] => format!(
            "track {} was ripped {} samples off the rest of the disc",
            number(k),
            (s1 - s0).abs()
        ),
        (Some(_), _) => format!(
            "the drive {} {} samples {} and {} {} samples {}",
            what(s1 - s0),
            (s1 - s0).abs(),
            place(job, &bounds, at[0]),
            what(s2 - s1),
            (s2 - s1).abs(),
            place(job, &bounds, at[1]),
        ),
        (None, _) => format!(
            "the drive {} {} samples {}",
            what(slip.shift),
            slip.shift.abs(),
            place(job, &bounds, at[0])
        ),
    };
    Ok(conclude(
        job, &bounds, &splice, &checked, &did, dest, progress,
    ))
}

/// A realigned album checked against CTDB, with the CRC32 of each track as
/// ripped, to tell which tracks realigning changes.
struct Checked {
    analysis: repair::Analysis,
    ripped: Vec<Option<u32>>,
}

/// Checks the album realigned by `splice` against CTDB's entries with parity
/// stride `stride_frames` (the one slip points were searched with: other
/// entries' columns differ).
fn check(
    job: &AlbumJob,
    toc: &str,
    db: &Db,
    stride_frames: usize,
    splice: &Splice,
    progress: &mut dyn FnMut(f32),
) -> Result<Checked> {
    let bounds = repair::track_bounds(job)?;
    let total = bounds[job.tracks.len()];
    let mut pass = repair::Pass::with_stride(job, toc, db, Some(stride_frames))?;
    let mut ripped = repair::TrackCrcs::new(bounds);
    let mut spliced = Spliced::new(splice.clone(), total);
    let notes = decode::decode_album(&job.tracks, false, |pos, frames| {
        ripped.push(frames);
        spliced.push(frames, &mut |f| pass.push(f));
        progress((pos + frames.len() as u64) as f32 / total as f32);
    });
    spliced.finish(&mut |f| pass.push(f));
    Ok(Checked {
        analysis: pass.finish(&notes, db)?,
        ripped: ripped.finish(),
    })
}

/// The corrections that make a realigned album match CTDB, if any do, and
/// the confidence of the entry it then matches: none if it already does.
fn repairs(analysis: &repair::Analysis) -> Option<(u32, &[Fix])> {
    analysis
        .fix_plan()
        .map(|(c, _, f)| (c, f))
        .or_else(|| analysis.verified().map(|(c, _)| (c, &[][..])))
}

/// Sums up a realignment (what the drive `did`) that `analysis` found
/// repairable and, with `dest`, writes the realigned, corrected tracks there.
fn conclude(
    job: &AlbumJob,
    bounds: &[u64],
    splice: &Splice,
    checked: &Checked,
    did: &str,
    dest: Option<&Path>,
    progress: &mut dyn FnMut(f32),
) -> (Outcome, String) {
    let analysis = &checked.analysis;
    let Some((confidence, fixes)) = repairs(analysis) else {
        return (Outcome::NotRepairable, analysis.summary());
    };
    let number = |k: usize| job.tracks[k].number.unwrap_or(k as u32 + 1);
    // The tracks realigning changes (moving silence doesn't), and any
    // others the corrections change.
    let mut tracks: Vec<usize> = splice
        .moved(bounds)
        .into_iter()
        .filter(|&k| analysis.track_crc(k).is_none() || analysis.track_crc(k) != checked.ripped[k])
        .collect();
    for t in repair::affected_tracks(bounds, fixes) {
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
    let summary = match fixes.len() {
        0 => format!(
            "Repairable: {did}; realigning track{} {list} matches CTDB exactly \
             (confidence {confidence})",
            plural(tracks.len()),
        ),
        n => format!(
            "Repairable: {did}; realigning track{} {list} and correcting {n} samples matches \
             CTDB (confidence {confidence})",
            plural(tracks.len()),
        ),
    };
    let Some(dest) = dest else {
        return (
            Outcome::Repairable,
            format!("{summary}; press r to write a repaired copy"),
        );
    };
    match write(job, bounds, splice, fixes, &tracks, dest, &mut |f| {
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
    }
}

/// Mismatched tracks to realign one by one, as a drive that loses sync
/// over and over leaves them: each may be shifted as a whole, or up to or
/// from a slip point inside it, by a few samples that AccurateRip shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Realign {
    /// The album's AccurateRip offset, in frames.
    pub offset: i64,
    /// The shifts to try, besides none.
    pub shifts: Vec<i64>,
    pub tracks: Vec<RealignTrack>,
}

/// A mismatched track (index): the shift at which its frame 450 lines up
/// with the rest of the album, if it does, and AccurateRip's checksums for
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealignTrack {
    pub track: usize,
    pub at450: Option<i64>,
    pub crcs: Vec<u32>,
}

/// A track realigned: its frames (counted as AccurateRip does, from the
/// album's offset) before `at` shifted by `before`, the rest by `after`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TrackFix {
    track: usize,
    before: i64,
    at: u64,
    after: i64,
}

/// A track's candidate realignments: `(before, after, at)`, with `at` in
/// `at.0..=at.1`. Shifted as a whole first, then with the slip after its
/// frame-450 window (which shows `before`) or before it (`after`).
fn candidate_fixes(t: &RealignTrack, shifts: &[i64], len: u64) -> Vec<(i64, i64, (u64, u64))> {
    let f450 = 450 * SECTOR_FRAMES;
    let mut all = vec![0];
    all.extend(shifts.iter().copied().filter(|&s| s != 0));
    let mut out = Vec::new();
    match t.at450 {
        Some(a) => {
            out.push((a, a, (0, 0)));
            for &c in all.iter().filter(|&&c| c != a) {
                if len > f450 + SECTOR_FRAMES {
                    out.push((a, c, (f450 + SECTOR_FRAMES, len)));
                }
                out.push((c, a, (1, f450.min(len))));
            }
        }
        None => {
            for &a in &all {
                for &c in &all {
                    out.push((a, c, if a == c { (0, 0) } else { (1, len) }));
                }
            }
        }
    }
    out
}

/// Finds the first of `candidates` (see [`candidate_fixes`]) at which a
/// track's AccurateRip checksum (v1 or v2) is one of `crcs`. `frame(i, s)`
/// is the track's frame `i` (counted from the album's offset) shifted by
/// `s`; positions outside `mults` don't count, as for AccurateRip.
/// Checksums are sums over frames, so each candidate's for every slip point
/// comes from running sums: the frames before it at one shift plus those
/// from it on at the other.
fn search_track(
    len: u64,
    mults: (u64, u64),
    frame: impl Fn(u64, i64) -> u32,
    candidates: &[(i64, i64, (u64, u64))],
    crcs: &[u32],
) -> Option<(i64, u64, i64)> {
    let mut shifts: Vec<i64> = candidates.iter().flat_map(|c| [c.0, c.1]).collect();
    shifts.sort_unstable();
    shifts.dedup();
    let idx = |s: i64| shifts.binary_search(&s).unwrap_or_default();
    let term = |i: u64, s: i64| -> (u32, u32) {
        let m = i + 1;
        if m < mults.0 || m > mults.1 {
            return (0, 0);
        }
        let prod = frame(i, s) as u64 * m;
        (prod as u32, (prod >> 32) as u32)
    };
    let add = |a: (u32, u32), b: (u32, u32)| (a.0.wrapping_add(b.0), a.1.wrapping_add(b.1));
    // Whole-track sums for each shift.
    let mut total = vec![(0u32, 0u32); shifts.len()];
    for i in 0..len {
        for (t, &s) in total.iter_mut().zip(&shifts) {
            *t = add(*t, term(i, s));
        }
    }
    let crc = |head: (u32, u32), tail: (u32, u32)| {
        let (lo, hi) = add(head, tail);
        [lo, lo.wrapping_add(hi)]
    };
    let hit = |v: [u32; 2]| v.iter().any(|c| crcs.contains(c));
    let zero = (0, 0);
    for &(a, _, _) in candidates.iter().filter(|c| c.0 == c.1) {
        if hit(crc(total[idx(a)], zero)) {
            return Some((a, 0, a));
        }
    }
    let moving: Vec<_> = candidates.iter().filter(|c| c.0 != c.1).collect();
    if moving.is_empty() {
        return None;
    }
    let to = moving.iter().map(|c| c.2.1).max().unwrap_or(0);
    // Sums of the frames before `p`, for each shift.
    let mut before = vec![(0u32, 0u32); shifts.len()];
    for p in 0..=to {
        for &&(a, c, (lo, hi)) in &moving {
            if (lo..=hi).contains(&p) {
                let (ta, tc) = (before[idx(a)], before[idx(c)]);
                let rest = total[idx(c)];
                let tail = (rest.0.wrapping_sub(tc.0), rest.1.wrapping_sub(tc.1));
                if hit(crc(ta, tail)) {
                    return Some((a, p, c));
                }
            }
        }
        if p < len {
            for (b, &s) in before.iter_mut().zip(&shifts) {
                *b = add(*b, term(p, s));
            }
        }
    }
    None
}

/// The album realigned by `fixes`: track `k`'s frames from the album's
/// offset on, `bounds[k] + offset`, shifted as each says.
fn realigned(bounds: &[u64], offset: i64, fixes: &[TrackFix]) -> Splice {
    let at = |b: u64, d: u64| (b as i64 + offset + d as i64).max(0) as u64;
    let mut pieces = Vec::new();
    let mut sorted = fixes.to_vec();
    sorted.sort_by_key(|f| f.track);
    for f in sorted {
        pieces.push((at(bounds[f.track], 0), f.before));
        pieces.push((at(bounds[f.track], f.at), f.after));
        pieces.push((at(bounds[f.track + 1], 0), 0));
    }
    // A later piece at the same frame takes over.
    pieces.sort_by_key(|p| p.0);
    Splice { pieces }
}

/// What realigning a track did, for a message.
fn describe(job: &AlbumJob, f: &TrackFix) -> String {
    let number = job.tracks[f.track].number.unwrap_or(f.track as u32 + 1);
    let secs = f.at / (SECTOR_FRAMES * 75);
    match f.before == f.after {
        true => format!("track {number} is {} samples off", f.after.abs()),
        false => format!(
            "track {number} {} {} samples at {}:{:02}",
            what(f.after - f.before),
            (f.after - f.before).abs(),
            secs / 60,
            secs % 60
        ),
    }
}

/// Realigns mismatched tracks one by one (see [`Realign`]): each at the
/// shifts at which its AccurateRip checksum matches, if any do, and one
/// more where CTDB's parity says. Checks the album realigned so against
/// CTDB and, if it matches or is repairable and `dest` is given, writes the
/// realigned, corrected tracks there.
pub fn realign(
    job: &AlbumJob,
    toc: &str,
    db: &Db,
    plan: &Realign,
    dest: Option<&Path>,
    progress: &mut dyn FnMut(f32),
) -> (Outcome, String) {
    realign_attempt(job, toc, db, plan, dest, progress).unwrap_or_else(|e| repair::failed(&e))
}

fn realign_attempt(
    job: &AlbumJob,
    toc: &str,
    db: &Db,
    plan: &Realign,
    dest: Option<&Path>,
    progress: &mut dyn FnMut(f32),
) -> Result<(Outcome, String)> {
    let n = job.tracks.len();
    if plan.tracks.is_empty() || plan.tracks.iter().any(|t| t.track >= n) {
        bail!("no tracks to realign");
    }
    let bounds = repair::track_bounds(job)?;
    let total = bounds[n];
    let number = |k: usize| job.tracks[k].number.unwrap_or(k as u32 + 1);
    let rerip = |ks: &[usize]| {
        format!(
            "re-rip track{} {}",
            plural(ks.len()),
            ks.iter()
                .map(|&k| number(k).to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let all: Vec<usize> = plan.tracks.iter().map(|t| t.track).collect();
    let mut entries: Vec<_> = db::ctdb_exact(db, toc)
        .context("CTDB lookup")?
        .into_iter()
        .filter(|e| e.parity_url.is_some() && e.crc32.is_some())
        .collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.confidence));
    let Some(stride_frames) = entries.first().map(repair::entry_stride) else {
        return Ok((
            Outcome::NotRepairable,
            format!(
                "Not repairable: CTDB has no repair data for this disc; {}",
                rerip(&all)
            ),
        ));
    };
    entries.retain(|e| repair::entry_stride(e) == stride_frames);

    // 1. Each track's own slip, by its AccurateRip checksum. The tracks'
    // frames at every shift tried, gathered in one decode.
    let reach = plan.shifts.iter().map(|s| s.abs()).max().unwrap_or(0)
        + plan
            .tracks
            .iter()
            .filter_map(|t| t.at450)
            .map(i64::abs)
            .max()
            .unwrap_or(0);
    let windows: Vec<(i64, i64)> = plan
        .tracks
        .iter()
        .map(|t| {
            let start = bounds[t.track] as i64 + plan.offset - reach;
            (start, bounds[t.track + 1] as i64 + plan.offset + reach)
        })
        .collect();
    let mut bufs: Vec<Option<Vec<u32>>> = vec![None; plan.tracks.len()];
    let mut found: Vec<Option<TrackFix>> = vec![None; plan.tracks.len()];
    let search = |j: usize, buf: &[u32]| -> Option<TrackFix> {
        let t = &plan.tracks[j];
        let k = t.track;
        let len = bounds[k + 1] - bounds[k];
        let mults = (
            if k == 0 { crate::crc::SKIP_FRAMES } else { 1 },
            if k == n - 1 {
                len.saturating_sub(crate::crc::SKIP_FRAMES)
            } else {
                len
            },
        );
        let frame = |i: u64, s: i64| buf[(i as i64 + reach + s) as usize];
        let candidates = candidate_fixes(t, &plan.shifts, len);
        search_track(len, mults, frame, &candidates, &t.crcs).map(|(before, at, after)| TrackFix {
            track: k,
            before,
            at,
            after,
        })
    };
    let notes = decode::decode_album(&job.tracks, false, |pos, frames| {
        let (p0, p1) = (pos as i64, (pos + frames.len() as u64) as i64);
        for (j, &(w0, w1)) in windows.iter().enumerate() {
            if p1 <= w0.max(0) || p0 >= w1 || found[j].is_some() {
                continue;
            }
            let buf = bufs[j].get_or_insert_with(|| vec![0; (w1 - w0) as usize]);
            let (a, b) = (p0.max(w0), p1.min(w1));
            if a < b {
                buf[(a - w0) as usize..(b - w0) as usize]
                    .copy_from_slice(&frames[(a - p0) as usize..(b - p0) as usize]);
            }
            // The track's window is complete (or the album ends in it).
            if p1 >= w1.min(total as i64)
                && let Some(buf) = bufs[j].take()
            {
                found[j] = search(j, &buf);
            }
        }
        progress((p1 as f32 / total as f32) / 3.0);
    });
    check_notes(&notes)?;
    let mut fixes: Vec<TrackFix> = found.iter().flatten().copied().collect();
    let left: Vec<usize> = (0..plan.tracks.len())
        .filter(|&j| found[j].is_none())
        .collect();

    // 2. One track left: where CTDB's parity says, the rest realigned.
    let geo = Geometry::new(total, stride_frames * 2);
    if geo.stridecount == 0 {
        bail!("the disc is too short for its repair data");
    }
    let mut tries: Vec<Vec<TrackFix>> = Vec::new();
    match left[..] {
        [] => tries.push(fixes.clone()),
        [j] => {
            let t = &plan.tracks[j];
            let k = t.track;
            let len = bounds[k + 1] - bounds[k];
            let candidates = candidate_fixes(t, &plan.shifts, len);
            // The album realigned but for track `k`, which is shifted as a
            // whole by each shift tried.
            let mut shifts: Vec<i64> = candidates.iter().flat_map(|c| [c.0, c.1]).collect();
            shifts.sort_unstable();
            shifts.dedup();
            let splices: Vec<Splice> = shifts
                .iter()
                .map(|&s| {
                    let mut with = fixes.clone();
                    with.push(TrackFix {
                        track: k,
                        before: s,
                        at: 0,
                        after: s,
                    });
                    realigned(&bounds, plan.offset, &with)
                })
                .collect();
            let samples = gather(job, geo, &splices, &mut |f| progress((1.0 + f) / 3.0))?;
            let idx = |s: i64| shifts.binary_search(&s).unwrap_or_default();
            let start = (bounds[k] as i64 + plan.offset).max(0) as u64;
            let mut scored: Vec<(u32, TrackFix)> = Vec::new();
            for e in &entries {
                let Some(url) = &e.parity_url else { continue };
                let npar = repair::entry_npar(e);
                let bytes = db::fetch(db, url).context("downloading CTDB parity")?;
                let Some(syn) = parity::bytes_to_syndrome(geo.stride, npar, &bytes) else {
                    continue;
                };
                for &(before, after, (lo, hi)) in &candidates {
                    let fix = |at| TrackFix {
                        track: k,
                        before,
                        at,
                        after,
                    };
                    if before == after {
                        scored.push((0, fix(0)));
                        continue;
                    }
                    let region = (start + lo, (start + hi).min(total));
                    if let Some((score, at)) =
                        locate_scored(&samples, (idx(before), idx(after)), geo, region, &syn, npar)
                    {
                        scored.push((score, fix(at - start)));
                    }
                }
                if scored.iter().any(|s| s.0 > 0) {
                    break;
                }
            }
            // The best located slips, then the track shifted as a whole.
            scored.sort_by_key(|s| std::cmp::Reverse(s.0));
            for (_, f) in scored.into_iter().take(3) {
                let mut with = fixes.clone();
                with.push(f);
                tries.push(with);
            }
        }
        _ => {
            let ks: Vec<usize> = left.iter().map(|&j| plan.tracks[j].track).collect();
            return Ok((
                Outcome::NotRepairable,
                format!(
                    "Not repairable: AccurateRip finds no slip in track{} {} to realign; {}",
                    plural(ks.len()),
                    ks.iter()
                        .map(|&k| number(k).to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                    rerip(&ks)
                ),
            ));
        }
    }

    // 3. The album realigned so, against CTDB.
    let mut last_why = String::from("no slip found in CTDB's repair data");
    for (i, with) in tries.iter().enumerate() {
        let splice = realigned(&bounds, plan.offset, with);
        let part = |f: f32| (1.0 + (i as f32 + f) / tries.len() as f32) / 3.0;
        let checked = check(job, toc, db, stride_frames, &splice, &mut |f| {
            progress(part(f))
        })?;
        if repairs(&checked.analysis).is_some() {
            fixes = with.clone();
            fixes.sort_by_key(|f| f.track);
            let did = fixes
                .iter()
                .map(|f| describe(job, f))
                .collect::<Vec<_>>()
                .join(", ");
            return Ok(conclude(
                job, &bounds, &splice, &checked, &did, dest, progress,
            ));
        }
        last_why = checked
            .analysis
            .summary()
            .trim_start_matches("Not repairable: ")
            .to_string();
    }
    Ok((
        Outcome::NotRepairable,
        format!(
            "Not repairable: realigned track by track, {last_why}; {}",
            rerip(&all)
        ),
    ))
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
    splice: &Splice,
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
    let mut spliced = Spliced::new(splice.clone(), total);
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
            let s = splice_all(&rip, Splice::new([10, 30], [0, 4, 0]), chunk);
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
            let found = locate(&s, (0, 1), geo, (4000, 12000), &db_syn, npar).expect("found");
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
                Splice::new([f1, f2], shift),
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
        let splice = Splice::new;
        assert_eq!(splice([150, 150], [0, 6, 6]).moved(&bounds), vec![1, 2, 3]);
        assert_eq!(splice([150, 150], [-6, 0, 0]).moved(&bounds), vec![0, 1]);
        assert_eq!(splice([200, 200], [0, 6, 6]).moved(&bounds), vec![2, 3]);
        assert_eq!(splice([100, 200], [0, 48, 0]).moved(&bounds), vec![1]);
        assert_eq!(splice([150, 250], [0, 48, 0]).moved(&bounds), vec![1, 2]);
    }

    /// AccurateRip's v1 and v2 checksums of `track`, all positions counting.
    fn ar_crcs(track: &[u32]) -> [u32; 2] {
        let (mut lo, mut hi) = (0u32, 0u32);
        for (i, &f) in track.iter().enumerate() {
            let p = f as u64 * (i as u64 + 1);
            lo = lo.wrapping_add(p as u32);
            hi = hi.wrapping_add((p >> 32) as u32);
        }
        [lo, lo.wrapping_add(hi)]
    }

    #[test]
    fn finds_track_slips_by_checksum() {
        let len = 40_000u64;
        let disc = noise(len as usize + 200, 3);
        let truth = &disc[100..100 + len as usize];
        let [v1, v2] = ar_crcs(truth);
        let t = |at450| RealignTrack {
            track: 1,
            at450,
            crcs: vec![v2],
        };
        let candidates = |at450| candidate_fixes(&t(at450), &[6, -6], len);
        // Ripped 6 frames late from frame 30_000 on, with 6 frames
        // repeated there: shifted back from there, it's the truth.
        // (and the next track's first frames after it).
        let rip: Vec<u32> = (0..len as usize + 12)
            .map(|i| {
                if i < 30_000 {
                    truth[i]
                } else {
                    disc[100 + i - 6]
                }
            })
            .collect();
        let frame = |i: u64, s: i64| {
            usize::try_from(i as i64 + s)
                .ok()
                .and_then(|j| rip.get(j))
                .copied()
                .unwrap_or(0)
        };
        let found = search_track(len, (1, len), frame, &candidates(Some(-6)), &[v2]);
        assert_eq!(found, None, "frame 450 is unshifted here");
        // Anywhere in the repeated frames will do.
        let found = search_track(len, (1, len), frame, &candidates(None), &[v1]);
        assert!(matches!(found, Some((0, 29_994..=30_000, 6))), "{found:?}");
        // A whole track ripped 6 frames early.
        let rip: Vec<u32> = (0..len as usize).map(|i| disc[100 + i - 6]).collect();
        let frame = |i: u64, s: i64| rip.get((i as i64 + s) as usize).copied().unwrap_or(0);
        let whole = search_track(len, (1, len), frame, &candidates(Some(6)), &[v2]);
        assert_eq!(whole, None, "the last 6 frames weren't ripped");
        let truth_crcs = ar_crcs(&truth[..len as usize - 6]);
        let found = search_track(
            len - 6,
            (1, len - 6),
            frame,
            &candidates(Some(6)),
            &truth_crcs,
        );
        assert_eq!(found, Some((6, 0, 6)));
    }

    #[test]
    fn candidates_follow_frame_450() {
        let f450 = 450 * SECTOR_FRAMES;
        let t = RealignTrack {
            track: 0,
            at450: Some(6),
            crcs: vec![],
        };
        let c = candidate_fixes(&t, &[6, -6], 1_000_000);
        assert_eq!(c[0], (6, 6, (0, 0)));
        assert!(c.contains(&(6, 0, (f450 + SECTOR_FRAMES, 1_000_000))));
        assert!(c.contains(&(0, 6, (1, f450))));
        assert!(c.contains(&(-6, 6, (1, f450))));
        assert!(c.iter().all(|&(a, b, _)| a == 6 || b == 6));
    }

    #[test]
    fn realigns_tracks_in_pieces() {
        let bounds = [0, 1000, 2000, 3000, 4000];
        let fixes = [
            TrackFix {
                track: 2,
                before: 6,
                at: 0,
                after: 6,
            },
            TrackFix {
                track: 0,
                before: -6,
                at: 400,
                after: 0,
            },
            TrackFix {
                track: 1,
                before: 6,
                at: 300,
                after: 0,
            },
        ];
        // Counted from the album's offset, +36.
        let s = realigned(&bounds, 36, &fixes);
        let at = |i| s.shift(i);
        assert_eq!((at(0), at(35), at(36), at(435), at(436)), (0, 0, -6, -6, 0));
        assert_eq!((at(1036), at(1335), at(1336)), (6, 6, 0));
        assert_eq!((at(2035), at(2036), at(3035), at(3036)), (0, 6, 6, 0));
        assert_eq!(s.moved(&bounds), vec![0, 1, 2, 3]);
        assert_eq!(s.least(), -6);
    }
}
