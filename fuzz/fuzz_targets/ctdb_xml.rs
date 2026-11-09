// SPDX-License-Identifier: GPL-3.0-or-later

//! CTDB lookup responses: entry attributes, their TOCs, base64 syndromes and
//! parity URLs.

#![no_main]

use cdverify_fuzz::db;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(xml) = std::str::from_utf8(data) else {
        return;
    };
    // A plausible disc to match the TOCs against.
    let offsets = [0, 15_000, 30_000, 45_000];
    let audio_end = 60_000;
    for e in db::parse_ctdb_entries(xml) {
        _ = db::layout_from_toc(&e.toc, &offsets, audio_end);
        _ = db::layout_from_toc(&e.toc, &offsets[..1], 15_000);
        if let Some(s) = &e.syndrome {
            _ = db::base64_decode(s);
        }
        if let Some(p) = &e.parity {
            _ = db::base64_decode(p);
        }
    }
    _ = db::parity_url(xml);
    _ = db::base64_decode(xml);
});
