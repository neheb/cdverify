// SPDX-License-Identifier: GPL-3.0-or-later

//! JSON and plain-text reports.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::verify::{AlbumResult, TrackStatus, Verdict};

/// Writes `path` (JSON) and a text summary next to it. Returns both paths.
pub fn write(path: &Path, albums: &[AlbumResult]) -> Result<(PathBuf, PathBuf)> {
    std::fs::write(path, serde_json::to_string_pretty(albums)?)?;
    let txt = path.with_extension("txt");
    std::fs::write(&txt, text(albums))?;
    Ok((path.to_path_buf(), txt))
}

pub fn text(albums: &[AlbumResult]) -> String {
    let mut out = String::new();
    for v in Verdict::ALL {
        let list: Vec<&AlbumResult> = albums.iter().filter(|a| a.verdict == v).collect();
        let _ = writeln!(out, "== {} ({}) ==", v.label(), list.len());
        for a in list {
            let _ = writeln!(out, "{}  [{}]", a.title, a.reason);
            let _ = writeln!(out, "    {}", a.dir.display());
            if let Some(note) = &a.data_track {
                let _ = writeln!(out, "    {note}");
            }
            if let Some(d) = &a.diagnosis {
                let _ = writeln!(out, "    {d}");
            }
            if let Some(note) = &a.ctdb_repair {
                let _ = writeln!(out, "    {note}");
            }
            if let Some(r) = &a.repair {
                let _ = writeln!(out, "    Repair: {r}");
            }
            if let Some(url) = a.musicbrainz_url() {
                let _ = writeln!(out, "    {url}");
            }
            if let Some(tag) = &a.mb_tag_mismatch {
                let _ = writeln!(out, "    MUSICBRAINZ_DISCID tag differs: {tag}");
            }
            if v == Verdict::Accurate {
                continue;
            }
            for t in &a.tracks {
                if matches!(t.status, TrackStatus::Accurate | TrackStatus::AccurateTag) {
                    continue;
                }
                let crc = |c: Option<u32>| c.map_or("--------".into(), |c| format!("{c:08X}"));
                let _ = writeln!(
                    out,
                    "    {:>2}  {:<14} v1 {} v2 {} AR {} conf {:>3}  {}{}",
                    t.number,
                    t.status.label(),
                    crc(t.v1),
                    crc(t.v2),
                    crc(t.db_crc),
                    t.confidence,
                    t.path.file_name().unwrap_or_default().to_string_lossy(),
                    t.note
                        .as_deref()
                        .map(|n| format!("  ({n})"))
                        .unwrap_or_default(),
                );
            }
        }
        out.push('\n');
    }
    out
}
