// SPDX-License-Identifier: GPL-3.0-or-later

//! AccurateRip `dBAR-…bin` responses.

#![no_main]

use cdverify_fuzz::db;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&ntracks, bytes)) = data.split_first() else {
        return;
    };
    if let Ok(pressings) = db::parse(bytes, ntracks as usize) {
        for p in pressings {
            assert_eq!(p.entries.len(), ntracks as usize);
        }
    }
});
