// SPDX-License-Identifier: GPL-3.0-or-later

//! cdverify's parsers of untrusted input, compiled from its own sources (it
//! is a binary, with no library to link against). These modules depend only
//! on each other, so `crate::disc` and the like resolve as they do there.

#![allow(dead_code)]

#[path = "../../src/cue.rs"]
pub mod cue;
#[path = "../../src/db.rs"]
pub mod db;
#[path = "../../src/disc.rs"]
pub mod disc;
#[path = "../../src/parity.rs"]
pub mod parity;
