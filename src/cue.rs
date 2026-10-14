// SPDX-License-Identifier: GPL-3.0-or-later

//! CUE sheets of single-file CD images: a FLAC `CUESHEET` block, a
//! `CUESHEET` tag, or a `.cue` file next to the image.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::disc::SECTOR_FRAMES;

/// An audio track of an image: its number and where it starts (INDEX 01),
/// in frames from the start of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CueTrack {
    pub number: u32,
    pub start: u64,
}

const CUESHEET: u8 = 5;
/// Track number of the lead-out in a FLAC CUESHEET (CD-DA).
const LEAD_OUT: u8 = 170;

/// The audio tracks of a FLAC file's `CUESHEET` block, if it has one.
pub fn from_flac(path: &Path) -> Option<Vec<CueTrack>> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic).ok()?;
    if &magic != b"fLaC" {
        return None;
    }
    loop {
        let mut header = [0u8; 4];
        f.read_exact(&mut header).ok()?;
        let len = u32::from_be_bytes([0, header[1], header[2], header[3]]) as u64;
        if header[0] & 0x7f == CUESHEET {
            let mut data = Vec::new();
            f.by_ref().take(len).read_to_end(&mut data).ok()?;
            return parse_flac_block(&data);
        }
        if header[0] & 0x80 != 0 {
            return None;
        }
        f.seek(SeekFrom::Current(len as i64)).ok()?;
    }
}

/// Parses a CUESHEET metadata block.
fn parse_flac_block(data: &[u8]) -> Option<Vec<CueTrack>> {
    // Catalog number (128), lead-in (8), CD flag and reserved (259).
    let mut p = 128 + 8 + 259;
    let ntracks = *data.get(p)?;
    p += 1;
    let mut tracks = Vec::new();
    for _ in 0..ntracks {
        let offset = u64::from_be_bytes(data.get(p..p + 8)?.try_into().ok()?);
        let number = *data.get(p + 8)?;
        // ISRC (12), then the type bit: set for non-audio tracks.
        let audio = data.get(p + 21)? & 0x80 == 0;
        let nindex = *data.get(p + 35)? as usize;
        p += 36;
        let mut index1 = None;
        let mut first = None;
        for _ in 0..nindex {
            let idx_off = u64::from_be_bytes(data.get(p..p + 8)?.try_into().ok()?);
            let idx = *data.get(p + 8)?;
            first.get_or_insert(idx_off);
            if idx == 1 {
                index1 = Some(idx_off);
            }
            p += 12;
        }
        if number == LEAD_OUT || number == 255 || !audio {
            continue;
        }
        tracks.push(CueTrack {
            number: number as u32,
            start: offset + index1.or(first)?,
        });
    }
    (!tracks.is_empty()).then_some(tracks)
}

/// Parses a text CUE sheet describing a single file. Returns `None` for
/// sheets with no audio tracks or more than one `FILE` (one file per track
/// isn't an image).
pub fn parse_text(text: &str) -> Option<Vec<CueTrack>> {
    let mut files = 0;
    let mut tracks = Vec::new();
    let mut current: Option<(u32, bool)> = None;
    for line in text.lines() {
        let mut words = line.split_whitespace();
        match words.next().map(str::to_ascii_uppercase).as_deref() {
            Some("FILE") => files += 1,
            Some("TRACK") => {
                let number = words.next()?.parse().ok()?;
                let audio = words
                    .next()
                    .is_some_and(|t| t.eq_ignore_ascii_case("AUDIO"));
                current = Some((number, audio));
            }
            Some("INDEX") => {
                let idx: u32 = words.next()?.parse().ok()?;
                if idx == 1
                    && let Some((number, true)) = current
                {
                    tracks.push(CueTrack {
                        number,
                        start: msf(words.next()?)? * SECTOR_FRAMES,
                    });
                }
            }
            _ => {}
        }
    }
    (files <= 1 && !tracks.is_empty()).then_some(tracks)
}

/// The `FILE` a single-file CUE sheet names.
pub fn file_name(text: &str) -> Option<String> {
    let line = text
        .lines()
        .map(str::trim_start)
        .find(|l| l.get(..5).is_some_and(|w| w.eq_ignore_ascii_case("FILE ")))?;
    let rest = line[5..].trim();
    let name = match rest.strip_prefix('"') {
        Some(quoted) => &quoted[..quoted.find('"')?],
        None => rest.split_whitespace().next()?,
    };
    Some(name.to_string())
}

/// Reads a CUE file, which may be UTF-8 (with or without a BOM) or Latin-1.
pub fn read_text(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
    Some(match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    })
}

/// `mm:ss:ff` (75 frames per second) to sectors.
fn msf(s: &str) -> Option<u64> {
    let mut parts = s.split(':').map(|p| p.parse::<u64>().ok());
    let (m, sec, f) = (parts.next()??, parts.next()??, parts.next()??);
    Some((m * 60 + sec) * 75 + f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_sheet() {
        let text = "REM GENRE Rock\r\nFILE \"Some Album.wav\" WAVE\r\n  TRACK 01 AUDIO\r\n    \
                    INDEX 00 00:00:00\r\n    INDEX 01 00:00:32\r\n  TRACK 02 AUDIO\r\n    \
                    INDEX 00 03:59:70\r\n    INDEX 01 04:01:00\r\n";
        assert_eq!(
            parse_text(text),
            Some(vec![
                CueTrack {
                    number: 1,
                    start: 32 * 588
                },
                CueTrack {
                    number: 2,
                    start: (241 * 75) * 588
                },
            ])
        );
        assert_eq!(file_name(text).as_deref(), Some("Some Album.wav"));
    }

    #[test]
    fn rejects_one_file_per_track() {
        let text = "FILE \"01.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n\
                    FILE \"02.wav\" WAVE\nTRACK 02 AUDIO\nINDEX 01 00:00:00\n";
        assert_eq!(parse_text(text), None);
    }

    #[test]
    fn parses_flac_block() {
        let mut b = vec![0u8; 128 + 8 + 259];
        b.push(3);
        let mut track = |offset: u64, number: u8, indexes: &[(u64, u8)]| {
            b.extend(offset.to_be_bytes());
            b.push(number);
            b.extend([0u8; 12 + 14]);
            b.push(indexes.len() as u8);
            for &(o, i) in indexes {
                b.extend(o.to_be_bytes());
                b.push(i);
                b.extend([0u8; 3]);
            }
        };
        track(0, 1, &[(0, 0), (588 * 33, 1)]);
        track(588 * 1000, 2, &[(0, 1)]);
        track(588 * 2000, LEAD_OUT, &[]);
        assert_eq!(
            parse_flac_block(&b),
            Some(vec![
                CueTrack {
                    number: 1,
                    start: 588 * 33
                },
                CueTrack {
                    number: 2,
                    start: 588 * 1000
                },
            ])
        );
    }
}
