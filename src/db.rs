// SPDX-License-Identifier: GPL-3.0-or-later

//! AccurateRip database lookup and response parsing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use serde::Serialize;

use crate::disc::DiscId;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Entry {
    pub confidence: u8,
    pub crc: u32,
    pub crc450: u32,
}

/// One block of the response: the results of one pressing/submission set.
#[derive(Debug, Clone, Serialize)]
pub struct Pressing {
    pub entries: Vec<Entry>,
}

pub type Pressings = Arc<Vec<Pressing>>;

/// Parses a `dBAR-*.bin` response. Blocks whose track count differs from the
/// queried disc are ignored.
pub fn parse(bytes: &[u8], ntracks: usize) -> Result<Vec<Pressing>> {
    let mut out = Vec::new();
    let mut p = 0;
    while p < bytes.len() {
        if bytes.len() - p < 13 {
            bail!("truncated AccurateRip header");
        }
        let n = bytes[p] as usize;
        p += 13;
        if bytes.len() - p < n * 9 {
            bail!("truncated AccurateRip entries");
        }
        let entries = (0..n)
            .map(|i| {
                let e = &bytes[p + i * 9..p + i * 9 + 9];
                Entry {
                    confidence: e[0],
                    crc: u32::from_le_bytes(e[1..5].try_into().unwrap()),
                    crc450: u32::from_le_bytes(e[5..9].try_into().unwrap()),
                }
            })
            .collect();
        p += n * 9;
        if n == ntracks {
            out.push(Pressing { entries });
        }
    }
    Ok(out)
}

pub struct Db {
    pub(crate) agent: ureq::Agent,
    cache: Mutex<HashMap<String, Option<Pressings>>>,
    /// CTDB responses by URL; one album asks for the same TOC several times.
    ctdb: Mutex<HashMap<String, Vec<CtdbEntry>>>,
}

impl Db {
    pub fn new() -> Db {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .user_agent(concat!("accuraterip-tui/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Db {
            agent,
            cache: Mutex::new(HashMap::new()),
            ctdb: Mutex::new(HashMap::new()),
        }
    }

    /// Returns `Ok(None)` when the disc is not in the database.
    pub fn lookup(&self, id: &DiscId) -> Result<Option<Pressings>> {
        let url = id.url();
        if let Some(hit) = self.cache.lock().unwrap().get(&url) {
            return Ok(hit.clone());
        }
        let result = match self.agent.get(&url).call() {
            Ok(mut resp) => {
                let bytes = resp.body_mut().read_to_vec()?;
                Some(Arc::new(parse(&bytes, id.ntracks)?))
            }
            Err(ureq::Error::StatusCode(404)) => None,
            Err(e) => return Err(e.into()),
        };
        self.cache.lock().unwrap().insert(url, result.clone());
        Ok(result)
    }
}

/// A real disc layout that the audio files alone don't reveal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TocLayout {
    /// Enhanced CD: data track after the audio, at `start`.
    DataLast { start: u32, leadout: u32 },
    /// Data track 1 (game CDs), with the audio starting at `audio_start`.
    DataFirst { audio_start: u32, leadout: u32 },
    /// Audio only, but track 1 starts at `start` after a pregap that wasn't ripped.
    Pregap { start: u32 },
}

/// One CTDB entry (a set of agreeing submissions) for a disc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtdbEntry {
    pub confidence: u32,
    pub toc: String,
    /// Has error-correction data that CUETools can use to repair a rip.
    pub has_parity: bool,
    /// CRC32 of the protected part of the disc audio.
    pub crc32: Option<u32>,
    /// Parity column width in frames.
    pub stride: Option<usize>,
    pub npar: Option<usize>,
    /// First column's syndrome (base64), used to find the offset.
    pub syndrome: Option<String>,
    /// Old entries: first column's parity (base64, npar 8).
    pub parity: Option<String>,
    /// Where the full parity file is.
    pub parity_url: Option<String>,
}

/// Queries CTDB by TOC (`start:start:…:leadout`, data tracks prefixed with
/// `-`). With `fuzzy`, discs with similar TOCs are returned too.
fn ctdb_lookup(db: &Db, toc: &str, fuzzy: bool) -> Result<Vec<CtdbEntry>> {
    let url = format!(
        "http://db.cuetools.net/lookup2.php?version=3&ctdb=1&fuzzy={}&metadata=none&toc={toc}",
        fuzzy as u8
    );
    if let Some(hit) = db.ctdb.lock().unwrap().get(&url) {
        return Ok(hit.clone());
    }
    let entries = match db.agent.get(&url).call() {
        Ok(mut resp) => parse_ctdb_entries(&resp.body_mut().read_to_string()?),
        // An exact TOC CTDB doesn't know gets a 404 rather than an empty
        // document; treat it like an empty result.
        Err(ureq::Error::StatusCode(404)) => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    db.ctdb.lock().unwrap().insert(url, entries.clone());
    Ok(entries)
}

/// Pulls the attributes of each `<entry …>` tag out of a CTDB response. Not a
/// real XML parser: it relies on CTDB's fixed output (double-quoted attributes,
/// no `>` inside values), and a tag it can't read is skipped.
fn parse_ctdb_entries(xml: &str) -> Vec<CtdbEntry> {
    let mut out = Vec::new();
    for entry in xml.split("<entry").skip(1) {
        let entry = entry.split('>').next().unwrap_or_default();
        let attr = |name: &str| {
            let key = format!(" {name}=\"");
            let rest = &entry[entry.find(&key)? + key.len()..];
            Some(&rest[..rest.find('"')?])
        };
        let Some(toc) = attr("toc") else { continue };
        out.push(CtdbEntry {
            confidence: attr("confidence").and_then(|c| c.parse().ok()).unwrap_or(0),
            toc: toc.to_string(),
            has_parity: attr("hasparity").is_some_and(|p| !p.is_empty()),
            crc32: attr("crc32").and_then(|c| u32::from_str_radix(c, 16).ok()),
            stride: attr("stride").and_then(|s| s.parse().ok()),
            npar: attr("npar").and_then(|s| s.parse().ok()),
            syndrome: attr("syndrome").map(str::to_string),
            parity: attr("parity").map(str::to_string),
            parity_url: attr("hasparity").filter(|p| !p.is_empty()).map(|p| {
                match p.starts_with('/') {
                    true => format!("http://db.cuetools.net{p}"),
                    false => p.to_string(),
                }
            }),
        });
    }
    out
}

/// The CTDB TOC string for our audio tracks laid out as `layout`.
pub fn ctdb_toc(offsets: &[u32], audio_end: u32, layout: Option<TocLayout>) -> String {
    let join = |base: u32| -> Vec<String> {
        offsets
            .iter()
            .chain(std::iter::once(&audio_end))
            .map(|o| (o + base).to_string())
            .collect()
    };
    let parts = match layout {
        None => join(0),
        Some(TocLayout::Pregap { start }) => join(start),
        Some(TocLayout::DataLast { start, leadout }) => {
            let mut p = join(0);
            p.pop();
            p.extend([format!("-{start}"), leadout.to_string()]);
            p
        }
        Some(TocLayout::DataFirst {
            audio_start,
            leadout,
        }) => {
            let mut p = vec!["-0".to_string()];
            p.extend(join(audio_start));
            p.pop();
            p.push(leadout.to_string());
            p
        }
    };
    parts.join(":")
}

/// Asks CTDB for full TOCs of discs whose audio tracks start at `offsets`
/// (relative to the first) and end at `audio_end`, and returns each layout
/// other than the plain one, most confident first.
pub fn ctdb_layouts(db: &Db, offsets: &[u32], audio_end: u32) -> Result<Vec<TocLayout>> {
    let entries = ctdb_lookup(db, &ctdb_toc(offsets, audio_end, None), true)?;
    Ok(layouts_from_entries(&entries, offsets, audio_end))
}

fn layouts_from_entries(entries: &[CtdbEntry], offsets: &[u32], audio_end: u32) -> Vec<TocLayout> {
    let mut found: Vec<(TocLayout, u32)> = Vec::new();
    for e in entries {
        if let Some(layout) = layout_from_toc(&e.toc, offsets, audio_end) {
            match found.iter_mut().find(|f| f.0 == layout) {
                Some(f) => f.1 += e.confidence,
                None => found.push((layout, e.confidence)),
            }
        }
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.1));
    found.into_iter().map(|f| f.0).collect()
}

/// CTDB's entries for exactly this TOC.
pub fn ctdb_exact(db: &Db, toc: &str) -> Result<Vec<CtdbEntry>> {
    Ok(ctdb_lookup(db, toc, false)?
        .into_iter()
        .filter(|e| e.toc == toc)
        .collect())
}

/// Downloads a CTDB parity file.
pub fn fetch(db: &Db, url: &str) -> Result<Vec<u8>> {
    Ok(db
        .agent
        .get(url)
        .call()?
        .body_mut()
        .with_config()
        .limit(16 << 20)
        .read_to_vec()?)
}

/// Standard base64 (CTDB attributes).
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let bytes: Vec<u8> = s
        .bytes()
        .filter(|&c| c != b'=' && !c.is_ascii_whitespace())
        .collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= (val(c)? as u32) << (18 - 6 * i);
        }
        out.extend(n.to_be_bytes()[1..chunk.len()].iter());
    }
    Some(out)
}

/// CTDB's entries for exactly this TOC: total confidence and whether any
/// carries repair data. `None` if CTDB doesn't know the disc.
pub fn ctdb_repair_info(db: &Db, toc: &str) -> Result<Option<(u32, bool)>> {
    let entries: Vec<CtdbEntry> = ctdb_lookup(db, toc, false)?
        .into_iter()
        .filter(|e| e.toc == toc)
        .collect();
    Ok((!entries.is_empty()).then(|| {
        (
            entries.iter().map(|e| e.confidence).sum(),
            entries.iter().any(|e| e.has_parity),
        )
    }))
}

/// Matches a CTDB TOC (`start:start:…:leadout`, data tracks prefixed with
/// `-`) against our audio tracks.
fn layout_from_toc(toc: &str, offsets: &[u32], audio_end: u32) -> Option<TocLayout> {
    let parts: Vec<&str> = toc.split(':').collect();
    let n = offsets.len();
    let num = |s: &str| s.parse::<u32>().ok();
    let audio_at = |parts: &[&str], base: u32| {
        parts
            .iter()
            .zip(offsets)
            .all(|(p, o)| num(p) == Some(o + base))
    };
    if parts.len() == n + 2 && parts[0] == "-0" {
        // Data track at 0, audio tracks, lead-out.
        let audio_start = num(parts[1])?;
        let leadout = num(parts[n + 1])?;
        (audio_at(&parts[1..], audio_start) && leadout == audio_start + audio_end).then_some(
            TocLayout::DataFirst {
                audio_start,
                leadout,
            },
        )
    } else if parts.len() == n + 2 {
        // Audio tracks (starting at 0), data track, lead-out.
        let start = num(parts[n].strip_prefix('-')?)?;
        let leadout = num(parts[n + 1])?;
        audio_at(&parts, 0).then_some(TocLayout::DataLast { start, leadout })
    } else if parts.len() == n + 1 {
        // Audio only: the same tracks shifted by a pregap before track 1.
        let start = num(parts[0])?;
        (start > 0 && audio_at(&parts, start) && num(parts[n])? == start + audio_end)
            .then_some(TocLayout::Pregap { start })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(n: u8, entries: &[(u8, u32, u32)]) -> Vec<u8> {
        let mut v = vec![n];
        v.extend_from_slice(&[0; 12]);
        for &(c, crc, f) in entries {
            v.push(c);
            v.extend_from_slice(&crc.to_le_bytes());
            v.extend_from_slice(&f.to_le_bytes());
        }
        v
    }

    #[test]
    fn parses_blocks() {
        let mut bytes = block(2, &[(5, 0x11223344, 0xaabbccdd), (3, 1, 2)]);
        bytes.extend(block(2, &[(1, 7, 8), (1, 9, 10)]));
        let p = parse(&bytes, 2).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].entries[0].confidence, 5);
        assert_eq!(p[0].entries[0].crc, 0x11223344);
        assert_eq!(p[0].entries[0].crc450, 0xaabbccdd);
        assert_eq!(p[1].entries[1].crc450, 10);
    }

    #[test]
    fn parses_ctdb_data_tracks() {
        let xml = r#"<ctdb>
 <entry confidence="16" crc32="1" toc="0:100:-11600:20000" trackcrcs="a b" />
 <entry confidence="1" toc="0:100:200" />
 <entry confidence="21" toc="0:100:-11600:20000" />
 <entry confidence="3" toc="0:100:-11600:19000" />
 <entry confidence="9" toc="0:150:-11600:20000" />
</ctdb>"#;
        assert_eq!(
            layouts_from_entries(&parse_ctdb_entries(xml), &[0, 100], 200),
            vec![
                TocLayout::DataLast {
                    start: 11600,
                    leadout: 20000
                },
                TocLayout::DataLast {
                    start: 11600,
                    leadout: 19000
                },
            ]
        );
    }

    #[test]
    fn parses_ctdb_pregap() {
        let xml = r#"<ctdb>
 <entry confidence="1" toc="0:100:200" />
 <entry confidence="243" toc="37:137:237" />
 <entry confidence="2" toc="37:140:237" />
</ctdb>"#;
        assert_eq!(
            layouts_from_entries(&parse_ctdb_entries(xml), &[0, 100], 200),
            vec![TocLayout::Pregap { start: 37 }]
        );
    }

    #[test]
    fn parses_ctdb_leading_data_track() {
        let xml = r#"<ctdb>
 <entry confidence="10" toc="-0:5000:5100:5200" />
 <entry confidence="7" toc="-0:6000:6100:6200" />
 <entry confidence="9" toc="-0:6000:6150:6200" />
</ctdb>"#;
        assert_eq!(
            layouts_from_entries(&parse_ctdb_entries(xml), &[0, 100], 200),
            vec![
                TocLayout::DataFirst {
                    audio_start: 5000,
                    leadout: 5200
                },
                TocLayout::DataFirst {
                    audio_start: 6000,
                    leadout: 6200
                },
            ]
        );
    }

    #[test]
    fn parses_ctdb_parity() {
        let xml = r#"<ctdb>
 <entry confidence="483" hasparity="http://p.cuetools.net/11200" npar="16" toc="0:5:9" />
 <entry confidence="1" npar="8" parity="f/QC" toc="0:5:9" />
 <entry confidence="2" toc="0:5:9" />
</ctdb>"#;
        assert_eq!(base64_decode("Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(base64_decode("Zm8=").unwrap(), b"fo");
        let e = parse_ctdb_entries(xml);
        assert_eq!(
            e.iter()
                .map(|e| (e.confidence, e.has_parity))
                .collect::<Vec<_>>(),
            [(483, true), (1, false), (2, false)]
        );
    }

    #[test]
    fn builds_ctdb_tocs() {
        let (offsets, end) = (&[0, 100][..], 200);
        assert_eq!(ctdb_toc(offsets, end, None), "0:100:200");
        assert_eq!(
            ctdb_toc(offsets, end, Some(TocLayout::Pregap { start: 37 })),
            "37:137:237"
        );
        assert_eq!(
            ctdb_toc(
                offsets,
                end,
                Some(TocLayout::DataLast {
                    start: 11600,
                    leadout: 20000
                })
            ),
            "0:100:-11600:20000"
        );
        assert_eq!(
            ctdb_toc(
                offsets,
                end,
                Some(TocLayout::DataFirst {
                    audio_start: 5000,
                    leadout: 5200
                })
            ),
            "-0:5000:5100:5200"
        );
        // Round trip through the parser.
        for layout in [
            TocLayout::Pregap { start: 37 },
            TocLayout::DataLast {
                start: 11600,
                leadout: 20000,
            },
            TocLayout::DataFirst {
                audio_start: 5000,
                leadout: 5200,
            },
        ] {
            assert_eq!(
                layout_from_toc(&ctdb_toc(offsets, end, Some(layout)), offsets, end),
                Some(layout)
            );
        }
    }

    #[test]
    fn rejects_truncated() {
        let bytes = block(2, &[(5, 1, 2)]);
        assert!(parse(&bytes, 2).is_err());
    }
}
