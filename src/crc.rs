// SPDX-License-Identifier: GPL-3.0-or-later

//! AccurateRip v1/v2 checksums, frame-450 checksums and offset handling.
//!
//! Samples are stereo frames packed as `left | right << 16`. Positions are
//! measured in frames from the start of the disc (track 1, index 1).

use std::sync::Arc;

use crate::disc::SECTOR_FRAMES;

/// Frames skipped at the start of the first and end of the last track.
pub const SKIP_FRAMES: u64 = 5 * SECTOR_FRAMES;
/// Largest pressing offset AccurateRip can detect (in frames).
pub const MAX_OFFSET: i64 = SKIP_FRAMES as i64 - 1;

const FRAME450_START: u64 = 450 * SECTOR_FRAMES;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrackCrc {
    pub v1: u32,
    pub v2: u32,
}

/// Accumulates v1/v2 checksums of every track as if the rip had been made
/// with an extra `offset` frames of read offset. A track at offset `o` covers
/// disc positions `[start + o, end + o)`, with silence outside the disc.
pub struct OffsetAccum {
    pub offset: i64,
    bounds: Arc<[u64]>,
    track: usize,
    lo: Vec<u32>,
    hi: Vec<u32>,
}

impl OffsetAccum {
    /// `bounds` holds the start of each track followed by the lead-out.
    pub fn new(offset: i64, bounds: Arc<[u64]>) -> Self {
        let n = bounds.len() - 1;
        OffsetAccum {
            offset,
            bounds,
            track: 0,
            lo: vec![0; n],
            hi: vec![0; n],
        }
    }

    /// Feeds frames starting at disc position `pos`. Calls must be in order.
    pub fn push(&mut self, pos: u64, frames: &[u32]) {
        let n = self.lo.len();
        let last = n - 1;
        // Shifted position of frames[0]; frames before 0 are off the disc.
        let q0 = pos as i64 - self.offset;
        let mut i = (-q0).max(0) as usize;
        // Splits the frames into runs within one track, so the inner loop is
        // a plain multiply-accumulate.
        while i < frames.len() {
            let q = (q0 + i as i64) as u64;
            while self.track < n && q >= self.bounds[self.track + 1] {
                self.track += 1;
            }
            if self.track == n {
                return;
            }
            let k = self.track;
            let start = self.bounds[k];
            let end = self.bounds[k + 1];
            let run_end = i + (end - q).min((frames.len() - i) as u64) as usize;
            // Multipliers (1-based position in the track) that count.
            let lo_mult = if k == 0 { SKIP_FRAMES } else { 1 };
            let hi_mult = if k == last {
                (end - start).saturating_sub(SKIP_FRAMES)
            } else {
                end - start
            };
            let first = q - start + 1;
            let a = i + lo_mult.saturating_sub(first) as usize;
            let b = run_end.min(i + (hi_mult + 1).saturating_sub(first) as usize);
            if a < b {
                // Track lengths fit in 32 bits, so a 32×32→64 multiply does.
                let mut mult = (first + (a - i) as u64) as u32;
                let (mut lo, mut hi) = (self.lo[k], self.hi[k]);
                for &s in &frames[a..b] {
                    let prod = s as u64 * mult as u64;
                    lo = lo.wrapping_add(prod as u32);
                    hi = hi.wrapping_add((prod >> 32) as u32);
                    mult = mult.wrapping_add(1);
                }
                self.lo[k] = lo;
                self.hi[k] = hi;
            }
            i = run_end;
        }
    }

    pub fn finish(&self) -> Vec<TrackCrc> {
        self.lo
            .iter()
            .zip(&self.hi)
            .map(|(&lo, &hi)| TrackCrc {
                v1: lo,
                v2: lo.wrapping_add(hi),
            })
            .collect()
    }
}

/// Captures, for each track, the frames around sector 450 needed to compute
/// the frame-450 checksum at every offset in `[-range, range]`.
pub struct Frame450Windows {
    range: i64,
    starts: Vec<i64>,
    bufs: Vec<Vec<u32>>,
}

impl Frame450Windows {
    pub fn new(range: i64, bounds: &[u64]) -> Self {
        let n = bounds.len() - 1;
        let starts = bounds[..n]
            .iter()
            .map(|&b| (b + FRAME450_START) as i64 - range)
            .collect();
        let len = SECTOR_FRAMES as usize + 2 * range as usize;
        Frame450Windows {
            range,
            starts,
            bufs: vec![vec![0; len]; n],
        }
    }

    pub fn push(&mut self, pos: u64, frames: &[u32]) {
        let (p0, p1) = (pos as i64, pos as i64 + frames.len() as i64);
        for (start, buf) in self.starts.iter().zip(self.bufs.iter_mut()) {
            let (w0, w1) = (*start, *start + buf.len() as i64);
            let (a, b) = (p0.max(w0), p1.min(w1));
            if a < b {
                buf[(a - w0) as usize..(b - w0) as usize]
                    .copy_from_slice(&frames[(a - p0) as usize..(b - p0) as usize]);
            }
        }
    }

    /// Frame-450 checksums of track `k` for offsets `-range..=range`
    /// (index `offset + range`).
    pub fn crc450_all(&self, k: usize) -> Vec<u32> {
        rolling_crc450(&self.bufs[k], self.range as usize)
    }

    pub fn range(&self) -> i64 {
        self.range
    }

    pub fn tracks(&self) -> usize {
        self.starts.len()
    }

    /// The disc position just past track `k`'s window.
    pub fn end(&self, k: usize) -> u64 {
        (self.starts[k] + self.bufs[k].len() as i64) as u64
    }
}

/// `out[i] = Σ_t buf[i + t] * (t + 1)` for t in one sector, using a rolling sum.
fn rolling_crc450(buf: &[u32], range: usize) -> Vec<u32> {
    let w = SECTOR_FRAMES as usize;
    let mut crc = 0u32;
    let mut sum = 0u32;
    for (t, &s) in buf[..w].iter().enumerate() {
        crc = crc.wrapping_add(s.wrapping_mul(t as u32 + 1));
        sum = sum.wrapping_add(s);
    }
    let mut out = Vec::with_capacity(2 * range + 1);
    out.push(crc);
    for i in 0..2 * range {
        let incoming = buf[i + w];
        crc = crc
            .wrapping_sub(sum)
            .wrapping_add(incoming.wrapping_mul(w as u32));
        sum = sum.wrapping_sub(buf[i]).wrapping_add(incoming);
        out.push(crc);
    }
    out
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

    /// Straightforward reference implementation (as in ARver).
    fn reference(track: &[u32], first: bool, last: bool) -> TrackCrc {
        let n = track.len() as u64;
        let from = if first { SKIP_FRAMES } else { 0 };
        let to = if last { n - SKIP_FRAMES } else { n };
        let (mut lo, mut hi) = (0u32, 0u32);
        for (i, &s) in track.iter().enumerate() {
            let m = i as u64 + 1;
            if m >= from && m <= to {
                let p = s as u64 * m;
                lo = lo.wrapping_add(p as u32);
                hi = hi.wrapping_add((p >> 32) as u32);
            }
        }
        TrackCrc {
            v1: lo,
            v2: lo.wrapping_add(hi),
        }
    }

    fn bounds(lens: &[u64]) -> Arc<[u64]> {
        let mut b = vec![0];
        for l in lens {
            b.push(b.last().unwrap() + l);
        }
        b.into()
    }

    #[test]
    fn offset_zero_matches_reference() {
        let lens = [SECTOR_FRAMES * 600, SECTOR_FRAMES * 20, SECTOR_FRAMES * 700];
        let disc = noise(lens.iter().sum::<u64>() as usize, 7);
        let b = bounds(&lens);
        let mut acc = OffsetAccum::new(0, b.clone());
        // Feed in odd-sized chunks to exercise boundaries.
        for (i, chunk) in disc.chunks(1000).enumerate() {
            acc.push(i as u64 * 1000, chunk);
        }
        let got = acc.finish();
        for k in 0..3 {
            let t = &disc[b[k] as usize..b[k + 1] as usize];
            assert_eq!(got[k], reference(t, k == 0, k == 2), "track {k}");
        }
    }

    #[test]
    fn offsets_match_naive() {
        let lens = [SECTOR_FRAMES * 6 + 7, SECTOR_FRAMES * 3, SECTOR_FRAMES * 11 + 1];
        let total: u64 = lens.iter().sum();
        let disc = noise(total as usize, 5);
        let b = bounds(&lens);
        for offset in [-3000, -2939, -37, -1, 0, 1, 588, 2939, 3000] {
            // Per frame, as the checksum is defined.
            let (mut lo, mut hi) = (vec![0u32; 3], vec![0u32; 3]);
            for (p, &s) in disc.iter().enumerate() {
                let q = p as i64 - offset;
                let Some(k) = (0..3).find(|&k| q >= b[k] as i64 && q < b[k + 1] as i64) else {
                    continue;
                };
                let mult = (q - b[k] as i64 + 1) as u64;
                let len = b[k + 1] - b[k];
                if (k == 0 && mult < SKIP_FRAMES) || (k == 2 && mult + SKIP_FRAMES > len) {
                    continue;
                }
                let prod = s as u64 * mult;
                lo[k] = lo[k].wrapping_add(prod as u32);
                hi[k] = hi[k].wrapping_add((prod >> 32) as u32);
            }
            for chunk in [1, 587, 4096, total as usize] {
                let mut acc = OffsetAccum::new(offset, b.clone());
                for (i, c) in disc.chunks(chunk).enumerate() {
                    acc.push((i * chunk) as u64, c);
                }
                let got = acc.finish();
                for k in 0..3 {
                    let want = TrackCrc {
                        v1: lo[k],
                        v2: lo[k].wrapping_add(hi[k]),
                    };
                    assert_eq!(got[k], want, "offset {offset} chunk {chunk} track {k}");
                }
            }
        }
    }

    #[test]
    fn offset_recovers_shifted_pressing() {
        // The "reference pressing" is the disc as-is; our rip is shifted by
        // `shift` frames (we read `shift` frames later), zero padded.
        let lens = [SECTOR_FRAMES * 500, SECTOR_FRAMES * 460];
        let total: u64 = lens.iter().sum();
        let pressing = noise(total as usize, 99);
        let shift: i64 = 37;
        let rip: Vec<u32> = (0..total as i64)
            .map(|p| {
                let q = p + shift;
                if q >= 0 && q < total as i64 {
                    pressing[q as usize]
                } else {
                    0
                }
            })
            .collect();
        let b = bounds(&lens);

        // Expected checksums are those of the pressing at offset 0.
        let mut want = OffsetAccum::new(0, b.clone());
        want.push(0, &pressing);
        let want = want.finish();

        // Frame-450 search over the rip finds the offset.
        let mut win = Frame450Windows::new(MAX_OFFSET, &b);
        win.push(0, &rip);
        for k in 0..2 {
            let crcs = win.crc450_all(k);
            let start = (b[k] + FRAME450_START) as usize;
            let mut target = 0u32;
            for t in 0..SECTOR_FRAMES as usize {
                target = target.wrapping_add(pressing[start + t].wrapping_mul(t as u32 + 1));
            }
            let found: Vec<i64> = crcs
                .iter()
                .enumerate()
                .filter(|(_, c)| **c == target)
                .map(|(i, _)| i as i64 - MAX_OFFSET)
                .collect();
            assert_eq!(found, vec![-shift], "track {k}");
        }

        // Full checksums at the found offset equal the pressing's.
        let mut acc = OffsetAccum::new(-shift, b);
        acc.push(0, &rip);
        assert_eq!(acc.finish(), want);
    }

    #[test]
    fn rolling_matches_naive() {
        let buf = noise(SECTOR_FRAMES as usize + 2 * 50, 3);
        let got = rolling_crc450(&buf, 50);
        for (i, g) in got.iter().enumerate() {
            let mut c = 0u32;
            for t in 0..SECTOR_FRAMES as usize {
                c = c.wrapping_add(buf[i + t].wrapping_mul(t as u32 + 1));
            }
            assert_eq!(*g, c);
        }
    }
}
