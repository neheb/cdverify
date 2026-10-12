// SPDX-License-Identifier: GPL-3.0-or-later

//! Finding audio files and grouping them into albums.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use symphonia::core::meta::{MetadataRevision, StandardTag};
use walkdir::WalkDir;

use crate::decode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Potentially decodable lossless file.
    Lossless,
    Lossy,
    /// Lossless (or unknown) format this tool cannot decode.
    Unsupported,
}

pub fn audio_kind(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "flac" | "wav" | "aif" | "aiff" | "aifc" | "m4a" => Kind::Lossless,
        "mp3" | "mp2" | "ogg" | "oga" | "opus" | "mpc" | "spx" | "aac" | "webm" => Kind::Lossy,
        "wv" | "ape" | "tak" | "tta" | "wma" | "dsf" | "dff" | "mka" | "shn" => Kind::Unsupported,
        _ => return None,
    })
}

/// Problems that make a track non-CD audio (see `app::is_non_cd`).
pub const LOSSY: &str = "lossy format";
pub const UNSUPPORTED: &str = "unsupported format";

/// Returns every folder containing audio files, with its files sorted by name.
pub fn find_folders(root: &Path) -> Vec<(PathBuf, Vec<PathBuf>)> {
    let mut map: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for entry in WalkDir::new(root).follow_links(true).into_iter().flatten() {
        let path = entry.path();
        if entry.file_type().is_file()
            && audio_kind(path).is_some()
            && let Some(dir) = path.parent()
        {
            map.entry(dir.to_path_buf())
                .or_default()
                .push(path.to_path_buf());
        }
    }
    map.into_iter()
        .map(|(dir, mut files)| {
            files.sort();
            (dir, files)
        })
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct TrackInfo {
    pub path: PathBuf,
    pub number: Option<u32>,
    pub total: Option<u32>,
    pub disc: Option<u32>,
    pub album: Option<String>,
    pub artist: Option<String>,
    pub sample_rate: Option<u32>,
    pub bits: Option<u32>,
    pub channels: Option<usize>,
    pub frames: Option<u64>,
    pub ar_result: Option<String>,
    pub mb_disc_id: Option<String>,
    /// Why this file cannot be checked, if it can't.
    pub problem: Option<String>,
}

pub fn probe_track(path: &Path) -> TrackInfo {
    let mut info = TrackInfo {
        path: path.to_path_buf(),
        ..Default::default()
    };
    match audio_kind(path) {
        Some(Kind::Lossy) => {
            info.problem = Some(LOSSY.into());
            return info;
        }
        Some(Kind::Unsupported) => {
            info.problem = Some(UNSUPPORTED.into());
            return info;
        }
        _ => {}
    }
    let mut opened = match decode::open(path, false) {
        Ok(o) => o,
        Err(e) => {
            info.problem = Some(format!("cannot decode: {e}"));
            return info;
        }
    };
    if let Some(track) = opened
        .format
        .tracks()
        .iter()
        .find(|t| t.id == opened.track_id)
    {
        info.frames = track.num_frames;
        if let Some(p) = track.codec_params.as_ref().and_then(|p| p.audio()) {
            info.sample_rate = p.sample_rate;
            info.bits = p.bits_per_sample;
            info.channels = p.channels.as_ref().map(|c| c.count());
        }
    }
    let mut md = opened.format.metadata();
    if let Some(rev) = md.current() {
        read_tags(rev, &mut info);
    }
    while !md.is_latest() {
        md.pop();
        if let Some(rev) = md.current() {
            read_tags(rev, &mut info);
        }
    }
    if info.number.is_none() {
        info.number = number_from_name(path);
    }
    info
}

fn read_tags(rev: &MetadataRevision, info: &mut TrackInfo) {
    let tags = rev
        .media
        .tags
        .iter()
        .chain(rev.per_track.iter().flat_map(|t| &t.metadata.tags));
    for tag in tags {
        match &tag.std {
            Some(StandardTag::TrackNumber(n)) => info.number = Some(*n as u32),
            Some(StandardTag::TrackTotal(n)) => info.total = Some(*n as u32),
            Some(StandardTag::DiscNumber(n)) => info.disc = Some(*n as u32),
            Some(StandardTag::Album(s)) => info.album = Some(s.to_string()),
            Some(StandardTag::AlbumArtist(s)) => info.artist = Some(s.to_string()),
            Some(StandardTag::Artist(s)) if info.artist.is_none() => {
                info.artist = Some(s.to_string())
            }
            Some(StandardTag::AccurateRipResult(s)) => info.ar_result = Some(s.to_string()),
            Some(StandardTag::MusicBrainzDiscId(s)) => info.mb_disc_id = Some(s.to_string()),
            _ if tag.raw.key.eq_ignore_ascii_case("ACCURATERIPRESULT") => {
                info.ar_result = Some(tag.raw.value.to_string())
            }
            _ => {}
        }
    }
}

/// Track number from names like "07 - Title" or "1.07 - Title" / "1-07 Title".
fn number_from_name(path: &Path) -> Option<u32> {
    let name = path.file_stem()?.to_str()?;
    let lead: String = name.chars().take_while(|c| c.is_ascii_digit()).collect();
    let rest = &name[lead.len()..];
    if let Some(after) = rest.strip_prefix(['.', '-']) {
        let second: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !second.is_empty() {
            return second.parse().ok();
        }
    }
    lead.parse().ok()
}

#[derive(Debug, Clone)]
pub struct AlbumJob {
    pub dir: PathBuf,
    pub title: String,
    pub tracks: Vec<TrackInfo>,
}

/// Splits one folder's tracks into albums by (album tag, disc number).
pub fn group(dir: &Path, tracks: Vec<TrackInfo>) -> Vec<AlbumJob> {
    let mut groups: BTreeMap<(Option<String>, Option<u32>), Vec<TrackInfo>> = BTreeMap::new();
    for t in tracks {
        groups.entry((t.album.clone(), t.disc)).or_default().push(t);
    }
    // Files whose tags couldn't be read (lossy etc.) join the only tagged
    // album when there is exactly one.
    if groups.len() == 2 && groups.contains_key(&(None, None)) {
        let untagged = groups.remove(&(None, None)).unwrap();
        groups.values_mut().next().unwrap().extend(untagged);
    }
    let multi_disc = groups.keys().filter_map(|k| k.1).collect::<Vec<_>>().len() > 1
        || groups.keys().any(|k| k.1.is_some_and(|d| d > 1));
    let folder = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    groups
        .into_iter()
        .map(|((album, disc), mut tracks)| {
            tracks.sort_by(|a, b| a.number.cmp(&b.number).then(a.path.cmp(&b.path)));
            let artist = tracks.iter().find_map(|t| t.artist.clone());
            let mut title = match (artist, album) {
                (Some(ar), Some(al)) => format!("{ar} - {al}"),
                (None, Some(al)) => al,
                _ => folder.clone(),
            };
            if multi_disc && let Some(d) = disc {
                title = format!("{title} (Disc {d})");
            }
            AlbumJob {
                dir: dir.to_path_buf(),
                title,
                tracks,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_from_names() {
        assert_eq!(number_from_name(Path::new("07 - Song.flac")), Some(7));
        assert_eq!(number_from_name(Path::new("1.02 - Tank!.flac")), Some(2));
        assert_eq!(number_from_name(Path::new("2-11 Song.flac")), Some(11));
        assert_eq!(number_from_name(Path::new("Song.flac")), None);
    }
}
