// SPDX-License-Identifier: GPL-3.0-or-later

//! Finding audio files and grouping them into albums.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::CODEC_ID_ALAC;
use symphonia::core::meta::{MetadataRevision, StandardTag};
use symphonia_common::apple::audio::alac::MagicCookie;
use walkdir::WalkDir;

use crate::cue::{self, CueTrack};
use crate::decode;
use crate::disc::SECTOR_FRAMES;

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
/// Start of the problem for files Symphonia can't open.
pub const CANNOT_DECODE: &str = "cannot decode";

fn is_cue(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("cue"))
}

/// Returns every folder containing audio files, with its audio and CUE files
/// sorted by name.
pub fn find_folders(root: &Path) -> Vec<(PathBuf, Vec<PathBuf>)> {
    let mut map: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for entry in WalkDir::new(root).follow_links(true).into_iter().flatten() {
        let path = entry.path();
        if entry.file_type().is_file()
            && (audio_kind(path).is_some() || is_cue(path))
            && let Some(dir) = path.parent()
        {
            map.entry(dir.to_path_buf())
                .or_default()
                .push(path.to_path_buf());
        }
    }
    map.into_iter()
        .filter(|(_, files)| files.iter().any(|f| !is_cue(f)))
        .map(|(dir, mut files)| {
            files.sort();
            (dir, files)
        })
        .collect()
}

/// The CUE files in the same folders as `paths`.
pub fn cue_files_beside(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs: Vec<&Path> = paths.iter().filter_map(|p| p.parent()).collect();
    dirs.sort();
    dirs.dedup();
    dirs.iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_cue(p) && p.is_file())
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
    /// For a track of a single-file image: where it starts in the file, in
    /// frames. `frames` is then the track's length, not the file's.
    pub image_start: Option<u64>,
    /// A `CUESHEET` tag (foobar2000 and CUETools embed the sheet this way).
    pub cue_tag: Option<String>,
    /// Why this file cannot be checked, if it can't.
    pub problem: Option<String>,
}

/// An ALAC stream's bit depth, from its magic cookie: Symphonia's MP4 reader
/// doesn't copy it into the codec parameters.
fn alac_bit_depth(p: &AudioCodecParameters) -> Option<u32> {
    if p.codec != CODEC_ID_ALAC {
        return None;
    }
    let cookie = MagicCookie::read(p.extra_data.as_deref()?).ok()?;
    Some(cookie.bit_depth.into())
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
        Err(e) if e.is::<decode::LossyCodec>() => {
            info.problem = Some(LOSSY.into());
            return info;
        }
        Err(e) => {
            info.problem = Some(format!("{CANNOT_DECODE}: {e}"));
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
            info.bits = p.bits_per_sample.or_else(|| alac_bit_depth(p));
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

/// Probes a folder's files (audio and CUE, as from [`find_folders`]): one
/// entry per track, with single-file images split into their tracks.
pub fn probe_files(files: &[PathBuf]) -> Vec<TrackInfo> {
    let sheets: Vec<(PathBuf, String)> = files
        .iter()
        .filter(|f| is_cue(f))
        .filter_map(|f| Some((f.clone(), cue::read_text(f)?)))
        .collect();
    let mut seen = std::collections::HashSet::new();
    files
        .iter()
        .filter(|f| !is_cue(f) && seen.insert(*f))
        .flat_map(|f| probe_file(f, &sheets))
        .collect()
}

/// Probes one audio file, splitting it into tracks if it's a CD image.
fn probe_file(path: &Path, sheets: &[(PathBuf, String)]) -> Vec<TrackInfo> {
    let mut info = probe_track(path);
    if info.problem.is_some() {
        return vec![info];
    }
    let is_flac = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("flac"));
    let sheet = is_flac
        .then(|| cue::from_flac(path))
        .flatten()
        .or_else(|| info.cue_tag.as_deref().and_then(cue::parse_text))
        .or_else(|| external_sheet(path, sheets));
    // A one-track sheet only matters when it says something the file
    // doesn't: its track number or a pregap.
    let Some(sheet) = sheet.filter(|s| s.len() > 1 || info.number.is_none() || s[0].start > 0)
    else {
        return vec![info];
    };
    match split_image(&info, &sheet) {
        Some(tracks) => tracks,
        None => {
            info.problem = Some("CUE sheet doesn't fit the audio".into());
            vec![info]
        }
    }
}

/// The tracks of a single-file CUE sheet next to `path` that names it (or a
/// file with the same stem, e.g. the WAV it was ripped to), or that has the
/// same stem itself.
fn external_sheet(path: &Path, sheets: &[(PathBuf, String)]) -> Option<Vec<CueTrack>> {
    let stem = |p: &Path| p.file_stem().map(|s| s.to_string_lossy().to_lowercase());
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    let own = stem(path)?;
    let named = |text: &str| {
        cue::file_name(text).is_some_and(|f| {
            let f = Path::new(&f.replace('\\', "/")).to_path_buf();
            f.file_name()
                .is_some_and(|n| n.to_string_lossy().to_lowercase() == name)
                || stem(&f).is_some_and(|s| s == own)
        })
    };
    let beside = |p: &PathBuf| p.parent() == path.parent();
    sheets
        .iter()
        .filter(|(p, _)| beside(p))
        .find(|(_, text)| named(text))
        .or_else(|| {
            sheets
                .iter()
                .find(|(p, _)| beside(p) && stem(p).is_some_and(|s| s == own))
        })
        .and_then(|(_, text)| cue::parse_text(text))
}

/// One entry per track of an image, or `None` if the sheet doesn't fit.
fn split_image(info: &TrackInfo, sheet: &[CueTrack]) -> Option<Vec<TrackInfo>> {
    let total = info.frames?;
    let ends = sheet.iter().skip(1).map(|t| t.start).chain([total]);
    sheet
        .iter()
        .zip(ends)
        .map(|(t, end)| {
            (t.start < end && t.start % SECTOR_FRAMES == 0).then(|| TrackInfo {
                number: Some(t.number),
                // The file's own tags describe the whole disc.
                total: None,
                ar_result: None,
                frames: Some(end - t.start),
                image_start: Some(t.start),
                ..info.clone()
            })
        })
        .collect()
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
            _ if tag.raw.key.eq_ignore_ascii_case("CUESHEET") => {
                info.cue_tag = Some(tag.raw.value.to_string())
            }
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

impl AlbumJob {
    /// Sectors before track 1 in a CD image (hidden track one audio), which
    /// the image's CUE sheet places on the disc.
    pub fn pregap(&self) -> u32 {
        self.tracks
            .first()
            .and_then(|t| t.image_start)
            .map_or(0, |s| (s / SECTOR_FRAMES) as u32)
    }
}

type GroupKey = (Option<String>, Option<u32>, Option<PathBuf>);

/// Splits one folder's tracks into albums by (album tag, disc number), with
/// each CD image an album of its own.
pub fn group(dir: &Path, tracks: Vec<TrackInfo>) -> Vec<AlbumJob> {
    let mut groups: BTreeMap<GroupKey, Vec<TrackInfo>> = BTreeMap::new();
    for t in tracks {
        let image = t.image_start.map(|_| t.path.clone());
        groups
            .entry((t.album.clone(), t.disc, image))
            .or_default()
            .push(t);
    }
    // Files whose tags couldn't be read (lossy etc.) join the only tagged
    // album when there is exactly one.
    if groups.len() == 2 && groups.contains_key(&(None, None, None)) {
        let untagged = groups.remove(&(None, None, None)).unwrap();
        groups.values_mut().next().unwrap().extend(untagged);
    }
    // Images that would otherwise share a title are told apart by file name.
    let shared = |album: &Option<String>, disc: Option<u32>| {
        groups
            .keys()
            .filter(|k| k.0 == *album && k.1 == disc)
            .count()
            > 1
    };
    let shared: Vec<bool> = groups.keys().map(|k| shared(&k.0, k.1)).collect();
    let multi_disc = groups.keys().filter_map(|k| k.1).collect::<Vec<_>>().len() > 1
        || groups.keys().any(|k| k.1.is_some_and(|d| d > 1));
    let folder = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    groups
        .into_iter()
        .zip(shared)
        .map(|(((album, disc, image), mut tracks), shared)| {
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
            if shared && let Some(stem) = image.as_deref().and_then(Path::file_stem) {
                title = format!("{title} [{}]", stem.to_string_lossy());
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
    fn splits_images() {
        let info = TrackInfo {
            path: "a.flac".into(),
            frames: Some(588 * 100),
            total: Some(1),
            ..Default::default()
        };
        let sheet = [
            CueTrack {
                number: 1,
                start: 588 * 10,
            },
            CueTrack {
                number: 2,
                start: 588 * 40,
            },
        ];
        let tracks = split_image(&info, &sheet).unwrap();
        let got: Vec<_> = tracks
            .iter()
            .map(|t| (t.number, t.image_start, t.frames, t.total))
            .collect();
        assert_eq!(
            got,
            [
                (Some(1), Some(588 * 10), Some(588 * 30), None),
                (Some(2), Some(588 * 40), Some(588 * 60), None)
            ]
        );
        let job = AlbumJob {
            dir: PathBuf::new(),
            title: String::new(),
            tracks,
        };
        assert_eq!(job.pregap(), 10);
        // A sheet running past the end of the audio doesn't fit.
        let short = TrackInfo {
            frames: Some(588 * 30),
            ..info
        };
        assert!(split_image(&short, &sheet).is_none());
    }

    #[test]
    fn numbers_from_names() {
        assert_eq!(number_from_name(Path::new("07 - Song.flac")), Some(7));
        assert_eq!(number_from_name(Path::new("1.02 - Tank!.flac")), Some(2));
        assert_eq!(number_from_name(Path::new("2-11 Song.flac")), Some(11));
        assert_eq!(number_from_name(Path::new("Song.flac")), None);
    }
}
