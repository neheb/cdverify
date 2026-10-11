// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2008-2025 Gregory S. Chudov (CUETools)
// Reed-Solomon code after Masayuki Miyazaki

//! CTDB (CUETools database) error correction: Reed-Solomon over GF(2^16).
//!
//! A port of CUETools' `CUETools.Parity` and the parity parts of
//! `CUETools.AccurateRip` (Gregory S. Chudov; RS code after Masayuki
//! Miyazaki), so that syndromes and repairs are bit-compatible with CTDB.
//!
//! The album is a stream of 16-bit words (left, right, left, …). Words are
//! split into `stride` columns; column `c` holds words `c, c + stride, …`.
//! Each column is one RS codeword over strides `1..=stridecount`; the first
//! stride ("lead-in") and the final `laststride` words ("lead-out") are kept
//! aside so the syndrome can be shifted by a pressing offset.

use std::sync::OnceLock;

/// Parity symbols per column computed (CTDB's maximum).
pub const MAX_NPAR: usize = 16;

pub struct Galois {
    exp: Vec<u16>,
    log: Vec<u16>,
}

pub const MAX: usize = 0xffff;

impl Galois {
    fn new() -> Galois {
        const POLYNOMIAL: u32 = 0x1100b;
        let mut exp = vec![0u16; MAX * 2];
        let mut log = vec![0u16; MAX + 1];
        let mut d: u32 = 1;
        for i in 0..MAX {
            exp[i] = d as u16;
            exp[MAX + i] = d as u16;
            log[d as usize] = i as u16;
            d <<= 1;
            if (d >> 16) & 1 != 0 {
                d = (d ^ POLYNOMIAL) & MAX as u32;
            }
        }
        Galois { exp, log }
    }

    pub fn get() -> &'static Galois {
        static G: OnceLock<Galois> = OnceLock::new();
        G.get_or_init(Galois::new)
    }

    #[inline]
    fn log(&self, a: u32) -> usize {
        self.log[a as usize] as usize
    }

    #[inline]
    fn mul(&self, a: u32, b: u32) -> u32 {
        if a == 0 || b == 0 {
            0
        } else {
            self.exp[self.log(a) + self.log(b)] as u32
        }
    }

    /// `a * α^b`, `b < MAX`.
    #[inline]
    fn mul_exp(&self, a: u32, b: usize) -> u32 {
        if a == 0 {
            0
        } else {
            self.exp[self.log(a) + b] as u32
        }
    }

    #[inline]
    fn div(&self, a: u32, b: u32) -> u32 {
        if a == 0 {
            0
        } else {
            self.exp[self.log(a) + MAX - self.log(b)] as u32
        }
    }

    /// `a / α^b`, `b < MAX`.
    #[inline]
    fn div_exp(&self, a: u32, b: usize) -> u32 {
        if a == 0 {
            0
        } else {
            self.exp[self.log(a) + MAX - b] as u32
        }
    }

    fn to_pos(&self, length: usize, a: u32) -> usize {
        length.wrapping_sub(1).wrapping_sub(self.log(a))
    }

    /// Generator polynomial Π(x + α^k), k < npar; index 0 is the x^(npar-1) term.
    fn encode_gx(&self, npar: usize) -> Vec<u32> {
        let mut gx = vec![0u32; npar];
        gx[npar - 1] = 1;
        for kou in 0..npar {
            let ex = self.exp[kou] as u32;
            for j in 0..npar - 1 {
                gx[j] = self.mul(gx[j], ex) ^ gx[j + 1];
            }
            gx[npar - 1] = self.mul(gx[npar - 1], ex);
        }
        gx
    }

    /// `table[b][0][i] = b * gx[i]`, `table[b][1][i] = (b << 8) * gx[i]`.
    fn encode_table(&self) -> Vec<[[u16; MAX_NPAR]; 2]> {
        let gx = self.encode_gx(MAX_NPAR);
        let mut t = vec![[[0u16; MAX_NPAR]; 2]; 256];
        for (b, row) in t.iter_mut().enumerate().skip(1) {
            for i in 0..MAX_NPAR {
                row[0][i] = self.mul(b as u32, gx[i]) as u16;
                row[1][i] = self.mul((b as u32) << 8, gx[i]) as u16;
            }
        }
        t
    }

    fn do_forney(&self, jisu: usize, ps: u32, sigma: &[u32], omega: &[u32]) -> u32 {
        let zlog = MAX - self.log(ps);
        let mut ov = omega[0];
        for (j, &o) in omega.iter().enumerate().take(jisu).skip(1) {
            ov ^= self.mul_exp(o, (zlog * j) % MAX);
        }
        let mut dv = sigma[1];
        let mut j = 2;
        while j < jisu {
            dv ^= self.mul_exp(sigma[j + 1], (zlog * j) % MAX);
            j += 2;
        }
        self.mul(ps, self.div(ov, dv))
    }

    /// `seki = a * b`, truncated to `seki.len()` terms.
    fn mul_poly(&self, seki: &mut [u32], a: &[u32], b: &[u32]) {
        seki.fill(0);
        for (ia, &av) in a.iter().enumerate() {
            if av == 0 {
                continue;
            }
            let loga = self.log(av);
            for (ib, &bv) in b.iter().enumerate().take(seki.len().saturating_sub(ia)) {
                if bv != 0 {
                    seki[ia + ib] ^= self.exp[loga + self.log(bv)] as u32;
                }
            }
        }
    }
}

/// Berlekamp–Massey (CUETools' `calcSigmaMBM`): error locator from the
/// syndrome. Returns the number of errors, or `None` if uncorrectable.
fn calc_sigma(g: &Galois, npar: usize, sigma: &mut [u32], syn: &[u32]) -> Option<usize> {
    let mut sg0 = vec![0u32; npar + 1];
    let mut sg1 = vec![0u32; npar + 1];
    let mut wk = vec![0u32; npar + 1];
    sg0[1] = 1;
    sg1[0] = 1;
    let mut jisu0 = 1usize;
    let mut jisu1 = 0usize;
    let mut m: isize = -1;
    for n in 0..npar {
        let mut d = syn[n];
        for i in 1..=jisu1 {
            d ^= g.mul(sg1[i], syn[n - i]);
        }
        if d != 0 {
            let logd = g.log(d);
            for i in 0..=n {
                wk[i] = sg1[i] ^ g.mul_exp(sg0[i], logd);
            }
            let js = (n as isize - m) as usize;
            if js > jisu1 {
                for i in 0..=jisu0 {
                    sg0[i] = g.div_exp(sg1[i], logd);
                }
                m = n as isize - jisu1 as isize;
                jisu1 = js;
                jisu0 = js;
            }
            sg1[..npar].copy_from_slice(&wk[..npar]);
        }
        for i in (1..=jisu0).rev() {
            sg0[i] = sg0[i - 1];
        }
        sg0[0] = 0;
        jisu0 += 1;
    }
    if sg1[jisu1] == 0 {
        return None;
    }
    let n = sigma.len().min(npar);
    sigma[..n].copy_from_slice(&sg1[..n]);
    Some(jisu1)
}

/// Chien search: the `jisu` roots of `sigma` within a codeword of length `n`.
fn chien_search(g: &Galois, pos: &mut [u32], n: usize, jisu: usize, sigma: &[u32]) -> bool {
    let mut last = sigma[1];
    let set_last = |pos: &mut [u32], last: u32| {
        if g.log(last) >= n {
            return false;
        }
        pos[0] = last;
        true
    };
    if jisu == 1 {
        return set_last(pos, last);
    }
    let mut sg: Vec<u32> = sigma[..=jisu].to_vec();
    let mut pos_idx = jisu - 1;
    for i in 0..n {
        let mut wk = 1u32;
        for &s in &sg[1..=jisu] {
            wk ^= s;
        }
        for (j, s) in sg.iter_mut().enumerate().skip(1) {
            *s = g.div_exp(*s, j);
        }
        if wk == 0 {
            let pv = g.exp[i] as u32;
            last ^= pv;
            pos[pos_idx] = pv;
            pos_idx -= 1;
            if pos_idx == 0 {
                return set_last(pos, last);
            }
        }
    }
    false
}

/// Stream geometry for an album of `samples` stereo frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// Columns, in 16-bit words (CTDB's `stride` attribute × 2).
    pub stride: usize,
    pub laststride: usize,
    pub stridecount: usize,
    /// Total 16-bit words.
    pub words: usize,
}

impl Geometry {
    pub fn new(samples: u64, stride: usize) -> Geometry {
        let words = samples as usize * 2;
        Geometry {
            stride,
            laststride: stride + words % stride,
            stridecount: (words / stride).saturating_sub(2),
            words,
        }
    }
}

/// Accumulates CTDB parity while the album streams through.
pub struct ParityEncoder {
    geo: Geometry,
    table: Vec<[[u16; MAX_NPAR]; 2]>,
    /// `stride` registers of `MAX_NPAR` words.
    parity: Vec<u16>,
    leadin: Vec<u16>,
    leadout: Vec<u16>,
    pos: usize,
}

impl ParityEncoder {
    pub fn new(geo: Geometry) -> ParityEncoder {
        ParityEncoder {
            geo,
            table: Galois::get().encode_table(),
            parity: vec![0; geo.stride * MAX_NPAR],
            leadin: vec![0; (geo.stride * 2).max(4096 * 4)],
            leadout: vec![0; (geo.stride + geo.laststride).max(4096 * 4)],
            pos: 0,
        }
    }

    /// Feeds stereo frames packed as `left | right << 16`, in order.
    pub fn push(&mut self, frames: &[u32]) {
        let geo = self.geo;
        let lo_end = geo.stride;
        let hi_end = geo.stride * (geo.stridecount + 1);
        for &f in frames {
            for word in [f as u16, (f >> 16) as u16] {
                let w = self.pos;
                if w < self.leadin.len() {
                    self.leadin[w] = word;
                }
                if w < geo.words {
                    let r = geo.words - 1 - w;
                    if r < self.leadout.len() {
                        self.leadout[r] = word;
                    }
                }
                if w >= lo_end && w < hi_end {
                    let col = w % geo.stride;
                    let reg = &mut self.parity[col * MAX_NPAR..(col + 1) * MAX_NPAR];
                    let fb = reg[0] ^ word;
                    let t0 = &self.table[(fb & 0xff) as usize][0];
                    let t1 = &self.table[(fb >> 8) as usize][1];
                    for k in 0..MAX_NPAR - 1 {
                        reg[k] = reg[k + 1] ^ t0[k] ^ t1[k];
                    }
                    reg[MAX_NPAR - 1] = t0[MAX_NPAR - 1] ^ t1[MAX_NPAR - 1];
                }
                self.pos += 1;
            }
        }
    }

    /// Syndromes of the first `strides` columns as if the rip were shifted
    /// by `offset` frames (`|offset| * 2 < stride`). Row-major `[strides][npar]`.
    pub fn syndrome(&self, npar: usize, strides: usize, offset: i64) -> Vec<u16> {
        let g = Galois::get();
        let geo = self.geo;
        let s = geo.stride as i64;
        let mut syn = parity_to_syndrome(strides, geo.stride, npar, &self.parity, -offset * 2);
        for part2 in 0..strides {
            let part = (part2 as i64 + offset * 2 + s).rem_euclid(s);
            let row = &mut syn[part2 * npar..(part2 + 1) * npar];
            if part < offset * 2 {
                let p = part as usize;
                for (i, v) in row.iter_mut().enumerate() {
                    let mut x = g.mul_exp(*v as u32, i);
                    x ^= self.leadout[geo.laststride - p - 1] as u32
                        ^ g.mul_exp(
                            self.leadin[geo.stride + p] as u32,
                            (i * geo.stridecount) % MAX,
                        );
                    *v = x as u16;
                }
            }
            if part >= s + offset * 2 {
                let p = part as usize;
                for (i, v) in row.iter_mut().enumerate() {
                    let mut x = *v as u32;
                    x ^= self.leadout[geo.laststride + geo.stride - p - 1] as u32
                        ^ g.mul_exp(self.leadin[p] as u32, (i * geo.stridecount) % MAX);
                    *v = g.div_exp(x, i) as u16;
                }
            }
        }
        syn
    }

    pub fn geometry(&self) -> Geometry {
        self.geo
    }
}

/// CUETools' `Parity2Syndrome` for our `MAX_NPAR`-wide parity registers.
fn parity_to_syndrome(
    strides: usize,
    stride2: usize,
    npar: usize,
    parity: &[u16],
    offset: i64,
) -> Vec<u16> {
    let g = Galois::get();
    let mut syn = vec![0u16; strides * npar];
    for y in 0..strides {
        let y1 = (y as i64 - offset + stride2 as i64).rem_euclid(stride2 as i64) as usize;
        let par = &parity[y1 * MAX_NPAR..(y1 + 1) * MAX_NPAR];
        let row = &mut syn[y * npar..(y + 1) * npar];
        for (x1, &lo) in par.iter().enumerate() {
            if lo != 0 {
                let llo = g.log(lo as u32) + MAX;
                for (x, v) in row.iter_mut().enumerate() {
                    *v ^= g.exp[llo - (1 + x1) * x];
                }
            }
        }
    }
    syn
}

/// CTDB's parity file / `syndrome` attribute layout (`[npar][stride]` LE
/// words) to row-major `[stride][npar]`.
pub fn bytes_to_syndrome(stride: usize, npar: usize, bytes: &[u8]) -> Option<Vec<u16>> {
    if bytes.len() < npar * stride * 2 {
        return None;
    }
    let mut syn = vec![0u16; stride * npar];
    for i in 0..npar {
        for j in 0..stride {
            let k = (j + i * stride) * 2;
            syn[i + j * npar] = u16::from_le_bytes([bytes[k], bytes[k + 1]]);
        }
    }
    Some(syn)
}

/// Converts a single-column parity (old CTDB entries' `parity` attribute) to
/// its syndrome.
pub fn parity_bytes_to_syndrome(npar: usize, bytes: &[u8]) -> Option<Vec<u16>> {
    if bytes.len() < npar * 2 {
        return None;
    }
    let g = Galois::get();
    let mut syn = vec![0u16; npar];
    for x1 in 0..npar {
        let lo = u16::from_le_bytes([bytes[x1 * 2], bytes[x1 * 2 + 1]]);
        if lo != 0 {
            let llo = g.log(lo as u32) + MAX;
            for (x, v) in syn.iter_mut().enumerate() {
                *v ^= g.exp[llo - (1 + x1) * x];
            }
        }
    }
    Some(syn)
}

/// Finds the frame offset at which our single-column syndrome matches the
/// database's (`db_syn`, `npar` words). Returns `(offset, exact)`; `exact`
/// is false when the best offset only matches up to correctable errors.
pub fn find_offset(enc: &ParityEncoder, db_syn: &[u16]) -> Option<(i64, bool)> {
    let g = Galois::get();
    let npar = db_syn.len();
    let geo = enc.geometry();
    let half = (geo.stride / 2) as i64;
    let mut best: Option<(i64, usize)> = None;
    let mut sigma = vec![0u32; npar / 2 + 2];
    let mut errpos = vec![0u32; npar];
    for offset in (1 - half)..half {
        let ours = enc.syndrome(npar, 1, -offset);
        let syn: Vec<u32> = ours
            .iter()
            .zip(db_syn)
            .map(|(a, b)| (a ^ b) as u32)
            .collect();
        if syn.iter().all(|&s| s == 0) {
            return Some((offset, true));
        }
        if let Some(n) = calc_sigma(g, npar, &mut sigma, &syn)
            && n > 0
            && n < best.map_or(npar / 2, |b| b.1)
            && chien_search(g, &mut errpos, geo.stridecount, n, &sigma)
        {
            best = Some((offset, n));
        }
    }
    best.map(|(o, _)| (o, false))
}

/// One corrected word: XOR `diff` into word `word` of the rip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fix {
    pub word: usize,
    pub diff: u16,
}

/// Works out the corrections that turn our rip (at `offset`) into the
/// database's audio, given its full syndrome (`[stride][npar]`). `None` if
/// the damage is beyond what the parity can correct.
pub fn compute_fixes(
    enc: &ParityEncoder,
    db_syn: &[u16],
    npar: usize,
    offset: i64,
) -> Option<Vec<Fix>> {
    let g = Galois::get();
    let geo = enc.geometry();
    let ours = enc.syndrome(npar, geo.stride, -offset);
    let mut fixes = Vec::new();
    let mut sigma = vec![0u32; npar / 2 + 2];
    let mut omega = vec![0u32; npar / 2 + 1];
    let mut errpos = vec![0u32; npar / 2];
    for part2 in 0..geo.stride {
        let syn: Vec<u32> = (0..npar)
            .map(|i| (ours[part2 * npar + i] ^ db_syn[part2 * npar + i]) as u32)
            .collect();
        if syn.iter().all(|&s| s == 0) {
            continue;
        }
        let n = calc_sigma(g, npar, &mut sigma, &syn)?;
        if n == 0 || n > npar / 2 || !chien_search(g, &mut errpos, geo.stridecount, n, &sigma) {
            return None;
        }
        g.mul_poly(&mut omega, &sigma, &syn);
        for &ep in &errpos[..n] {
            let pos = g.to_pos(geo.stridecount, ep) * geo.stride + part2;
            let word = (geo.stride + pos) as i64 - offset * 2;
            if word < 0 || word as usize >= geo.words {
                return None;
            }
            let diff = g.do_forney(n, ep, &sigma, &omega) as u16;
            fixes.push(Fix {
                word: word as usize,
                diff,
            });
        }
    }
    fixes.sort_by_key(|f| f.word);
    Some(fixes)
}

/// CTDB's CRC32 window: frames `[stride/2 + oi, N - laststride/2 + oi)` with
/// `oi = -offset`.
pub fn crc_window(geo: Geometry, offset: i64) -> (u64, u64) {
    let n = (geo.words / 2) as i64;
    let a = (geo.stride / 2) as i64 - offset;
    let b = n - (geo.laststride / 2) as i64 - offset;
    (a.max(0) as u64, b.max(0) as u64)
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

    fn encode(frames: &[u32], stride: usize) -> ParityEncoder {
        let mut enc = ParityEncoder::new(Geometry::new(frames.len() as u64, stride));
        for chunk in frames.chunks(777) {
            enc.push(chunk);
        }
        enc
    }

    fn apply(frames: &mut [u32], fixes: &[Fix]) {
        for f in fixes {
            let (i, hi) = (f.word / 2, f.word % 2 == 1);
            frames[i] ^= (f.diff as u32) << if hi { 16 } else { 0 };
        }
    }

    #[test]
    fn field_basics() {
        let g = Galois::get();
        for a in [1u32, 2, 0x1234, 0xffff] {
            for b in [1u32, 3, 0x8000, 0xabcd] {
                assert_eq!(g.div(g.mul(a, b), b), a);
            }
        }
    }

    #[test]
    fn repairs_damage() {
        let stride = 64; // words; small for the test
        let clean = noise(stride / 2 * 40 + 13, 1);
        let reference = encode(&clean, stride).syndrome(MAX_NPAR, stride, 0);

        let mut damaged = clean.clone();
        for &i in &[100usize, 101, 517, 900] {
            damaged[i] ^= 0x0040_1234;
        }
        let enc = encode(&damaged, stride);
        let (offset, exact) = find_offset(&enc, &reference[..MAX_NPAR]).unwrap();
        assert_eq!(offset, 0);
        assert!(!exact || reference[..MAX_NPAR] == enc.syndrome(MAX_NPAR, 1, 0)[..]);
        let fixes = compute_fixes(&enc, &reference, MAX_NPAR, 0).expect("repairable");
        apply(&mut damaged, &fixes);
        assert_eq!(damaged, clean);
    }

    #[test]
    fn repairs_at_offset() {
        let stride = 64;
        let len = stride / 2 * 40 + 13;
        let disc = noise(len + 40, 2);
        // The database's rip and ours differ by `shift` frames.
        let shift = 5usize;
        let theirs = &disc[shift..shift + len];
        let reference = encode(theirs, stride).syndrome(MAX_NPAR, stride, 0);

        let mut ours: Vec<u32> = disc[..len].to_vec();
        ours[400] ^= 0x0001_0001;
        let enc = encode(&ours, stride);
        let (offset, _) = find_offset(&enc, &reference[..MAX_NPAR]).unwrap();
        let fixes = compute_fixes(&enc, &reference, MAX_NPAR, offset).expect("repairable");
        apply(&mut ours, &fixes);
        // Inside the protected window the rip now equals the shifted reference.
        let geo = enc.geometry();
        let (a, b) = crc_window(geo, offset);
        for i in a..b {
            let j = (i as i64 + offset) as usize;
            assert_eq!(ours[i as usize], theirs[j], "frame {i}");
        }
        assert_eq!(offset, -(shift as i64));
    }

    #[test]
    fn rejects_too_much_damage() {
        let stride = 64;
        let clean = noise(stride / 2 * 40, 3);
        let reference = encode(&clean, stride).syndrome(MAX_NPAR, stride, 0);
        let mut damaged = clean.clone();
        // 9 errors in one column exceeds npar / 2 = 8.
        for k in 0..9 {
            damaged[stride / 2 * (2 + k)] ^= 7;
        }
        let enc = encode(&damaged, stride);
        assert!(compute_fixes(&enc, &reference, MAX_NPAR, 0).is_none());
    }
}
