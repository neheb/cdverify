// SPDX-License-Identifier: GPL-3.0-or-later

//! CUE sheets: text ones (`.cue` files, `CUESHEET` tags) and FLAC `CUESHEET`
//! metadata blocks.

#![no_main]

use cdverify_fuzz::cue;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    _ = cue::parse_flac_block(data);
    let text = String::from_utf8_lossy(data);
    _ = cue::parse_text(&text);
    _ = cue::file_name(&text);
});
