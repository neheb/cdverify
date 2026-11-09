// SPDX-License-Identifier: GPL-3.0-or-later

//! The CTDB parity decoder (Reed-Solomon over each column of the album).
//!
//! Most inputs encode some audio, corrupt a few of its words and check that
//! `find_offset` and `compute_fixes` restore it from the clean audio's
//! syndrome. Inputs with the top bit of the first byte set give the decoder
//! arbitrary syndrome bytes instead, as a hostile or broken server could.

#![no_main]

use cdverify_fuzz::parity::{
    self, Geometry, MAX_NPAR, ParityEncoder, bytes_to_syndrome, parity_bytes_to_syndrome,
};
use libfuzzer_sys::fuzz_target;

fn encoder(geo: Geometry, frames: &[u32]) -> ParityEncoder {
    let mut enc = ParityEncoder::new(geo);
    enc.push(frames);
    enc
}

fuzz_target!(|data: &[u8]| {
    let [mode, stride_lo, stride_hi, extra, rest @ ..] = data else {
        return;
    };
    if rest.is_empty() {
        return;
    }
    let raw = mode & 0x80 != 0;
    let npar = (*mode as usize % MAX_NPAR) + 1;
    // Small strides keep each run fast; real ones are 5880 frames.
    let stride_frames = 1 + u16::from_le_bytes([*stride_lo, *stride_hi]) as usize % 1024;
    // Whole strides plus a partial last one; raw mode also tries albums
    // shorter than a stride, which CTDB's own entries could claim.
    let samples = if raw {
        (*extra as usize * 37) % (4 * stride_frames) + 1
    } else {
        3 * stride_frames + *extra as usize * 13
    };
    let geo = Geometry::new(samples as u64, stride_frames * 2);
    let byte = |i: usize| rest[i % rest.len()];
    let clean: Vec<u32> = (0..samples)
        .map(|i| {
            u32::from_le_bytes([
                byte(4 * i),
                byte(4 * i + 1),
                byte(4 * i + 2),
                byte(4 * i + 3),
            ])
        })
        .collect();

    if raw {
        let enc = encoder(geo, &clean);
        let syn_bytes: Vec<u8> = (0..geo.stride * npar * 2).map(|i| byte(i + 7)).collect();
        if let Some(head) = parity_bytes_to_syndrome(npar, &syn_bytes) {
            _ = parity::find_offset(&enc, &head);
        }
        if let Some(head) = bytes_to_syndrome(1, npar, &syn_bytes) {
            _ = parity::find_offset(&enc, &head);
        }
        if let Some(full) = bytes_to_syndrome(geo.stride, npar, &syn_bytes) {
            let half = (geo.stride / 2) as i64;
            for offset in [0, 1 - half, half - 1] {
                _ = parity::compute_fixes(&enc, &full, npar, offset);
            }
        }
        return;
    }

    // Up to npar / 2 corrupted words in the parity-covered middle: always
    // correctable, whatever columns they land in.
    let first = geo.stride;
    let span = geo.stride * geo.stridecount;
    let mut damaged = clean.clone();
    let mut hit = Vec::new();
    for k in 0..npar / 2 {
        let word =
            first + (u16::from_le_bytes([byte(3 * k), byte(3 * k + 1)]) as usize * 31) % span;
        let diff = u16::from(byte(3 * k + 2)) | 1;
        if hit.contains(&word) {
            continue;
        }
        hit.push(word);
        damaged[word / 2] ^= u32::from(diff) << (16 * (word % 2));
    }

    let clean_enc = encoder(geo, &clean);
    let enc = encoder(geo, &damaged);
    let head = clean_enc.syndrome(npar, 1, 0);
    let full = clean_enc.syndrome(npar, geo.stride, 0);
    let found = parity::find_offset(&enc, &head);
    // Periodic audio (silence, say) matches at several offsets; any will do.
    if hit.is_empty() {
        assert!(
            found.is_some_and(|(_, exact)| exact),
            "undamaged audio must match exactly"
        );
    }
    let fixes = parity::compute_fixes(&enc, &full, npar, 0)
        .expect("at most npar / 2 errors must be correctable");
    let mut repaired = damaged;
    for f in &fixes {
        repaired[f.word / 2] ^= u32::from(f.diff) << (16 * (f.word % 2));
    }
    assert!(repaired == clean, "fixes must restore the clean audio");
});
