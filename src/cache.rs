// SPDX-License-Identifier: GPL-3.0-or-later

//! Persistent per-folder results cache.
//!
//! A folder's results are reused when its audio files are unchanged (same
//! names, sizes and modification times). Only final results are stored; see
//! [`AlbumResult::recheck`]. `--invert` runs keep their own entries, so
//! switching modes doesn't throw away the other mode's results.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::verify::AlbumResult;

const VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    pub name: String,
    pub size: u64,
    pub mtime_s: u64,
    pub mtime_ns: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub files: Vec<FileStamp>,
    pub albums: Vec<AlbumResult>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cache {
    version: u32,
    folders: HashMap<PathBuf, Entry>,
    /// Results of `--invert` runs.
    #[serde(default)]
    inverted: HashMap<PathBuf, Entry>,
    /// Which of the two this run uses.
    #[serde(skip)]
    ctdb_first: bool,
}

/// Stats `files`; `None` if any of them can't be read.
pub fn stamps(files: &[PathBuf]) -> Option<Vec<FileStamp>> {
    files
        .iter()
        .map(|f| {
            let md = std::fs::metadata(f).ok()?;
            let mtime = md.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
            Some(FileStamp {
                name: f.file_name()?.to_string_lossy().into_owned(),
                size: md.len(),
                mtime_s: mtime.as_secs(),
                mtime_ns: mtime.subsec_nanos(),
            })
        })
        .collect()
}

/// `$XDG_CACHE_HOME`, `~/.cache` or, on Windows, `%LOCALAPPDATA%`. Moves the
/// cache over from before the rename to cdverify if there is one.
pub fn default_path() -> PathBuf {
    let var = |name| std::env::var_os(name).map(PathBuf::from);
    let base = match cfg!(windows) {
        true => var("LOCALAPPDATA"),
        false => var("XDG_CACHE_HOME"),
    }
    .or_else(|| std::env::home_dir().map(|h| h.join(".cache")))
    .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("cdverify");
    let old = base.join("accuraterip-tui");
    if !dir.exists() && old.is_dir() {
        let _ = std::fs::rename(&old, &dir);
    }
    dir.join("cache.json")
}

impl Cache {
    /// Loads the cache, starting empty if it is missing or unreadable.
    /// `ctdb_first` picks the `--invert` entries.
    pub fn load(path: &Path, ctdb_first: bool) -> Cache {
        let mut c = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Cache>(&b).ok())
            .filter(|c| c.version == VERSION)
            .unwrap_or_else(|| Cache {
                version: VERSION,
                ..Cache::default()
            });
        c.ctdb_first = ctdb_first;
        c
    }

    /// This mode's entries, or with `other` the other mode's.
    fn entries(&self, other: bool) -> &HashMap<PathBuf, Entry> {
        match self.ctdb_first != other {
            true => &self.inverted,
            false => &self.folders,
        }
    }

    fn entries_mut(&mut self, other: bool) -> &mut HashMap<PathBuf, Entry> {
        match self.ctdb_first != other {
            true => &mut self.inverted,
            false => &mut self.folders,
        }
    }

    /// Writes atomically (temporary file + rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Cached results for `dir` if its files still match `files`.
    pub fn get(&self, dir: &Path, files: &[FileStamp]) -> Option<Vec<AlbumResult>> {
        get(self.entries(false), dir, files)
    }

    /// Like [`Cache::get`], for the other mode (`--both`).
    pub fn get_other(&self, dir: &Path, files: &[FileStamp]) -> Option<Vec<AlbumResult>> {
        get(self.entries(true), dir, files)
    }

    /// Records a folder's results, or forgets it if any album needs rechecking.
    pub fn update(&mut self, dir: PathBuf, files: Option<Vec<FileStamp>>, albums: &[AlbumResult]) {
        update(self.entries_mut(false), dir, files, albums);
    }

    /// Like [`Cache::update`], for the other mode (`--both`).
    pub fn update_other(
        &mut self,
        dir: PathBuf,
        files: Option<Vec<FileStamp>>,
        albums: &[AlbumResult],
    ) {
        update(self.entries_mut(true), dir, files, albums);
    }
}

fn get(
    entries: &HashMap<PathBuf, Entry>,
    dir: &Path,
    files: &[FileStamp],
) -> Option<Vec<AlbumResult>> {
    let e = entries.get(dir)?;
    (e.files == files).then(|| {
        let mut albums = e.albums.clone();
        for a in &mut albums {
            a.cached = true;
        }
        albums
    })
}

fn update(
    entries: &mut HashMap<PathBuf, Entry>,
    dir: PathBuf,
    files: Option<Vec<FileStamp>>,
    albums: &[AlbumResult],
) {
    match files {
        Some(files) if !albums.iter().any(|a| a.recheck) => {
            entries.insert(
                dir,
                Entry {
                    files,
                    albums: albums.to_vec(),
                },
            );
        }
        _ => {
            entries.remove(&dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::Verdict;

    fn album(recheck: bool) -> AlbumResult {
        AlbumResult {
            title: "t".into(),
            dir: "/x".into(),
            verdict: if recheck {
                Verdict::RipErrors
            } else {
                Verdict::Accurate
            },
            reason: String::new(),
            disc_id: None,
            url: None,
            pressings: 0,
            tracks: vec![],
            recheck,
            cached: false,
            mb_disc_id: None,
            mb_tag_mismatch: None,
            data_track: None,
            ctdb_repair: None,
            ctdb_known: false,
            repair_outcome: None,
            ctdb_toc: None,
            repair: None,
            diagnosis: None,
            ctdb_first: false,
        }
    }

    fn stamp(size: u64) -> FileStamp {
        FileStamp {
            name: "01.flac".into(),
            size,
            mtime_s: 1,
            mtime_ns: 2,
        }
    }

    #[test]
    fn hits_only_unchanged_final_results() {
        let mut c = Cache::default();
        c.update("/a".into(), Some(vec![stamp(10)]), &[album(false)]);
        c.update("/b".into(), Some(vec![stamp(10)]), &[album(true)]);
        let hit = c.get(Path::new("/a"), &[stamp(10)]).unwrap();
        assert!(hit[0].cached);
        assert!(c.get(Path::new("/a"), &[stamp(11)]).is_none());
        assert!(c.get(Path::new("/b"), &[stamp(10)]).is_none());
        // A folder that later needs rechecking is forgotten.
        c.update("/a".into(), Some(vec![stamp(10)]), &[album(true)]);
        assert!(c.get(Path::new("/a"), &[stamp(10)]).is_none());
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("artui-cache-test-{}", std::process::id()));
        let path = dir.join("cache.json");
        let mut c = Cache::load(&path, false);
        c.update("/a".into(), Some(vec![stamp(10)]), &[album(false)]);
        c.save(&path).unwrap();
        let mut inv = Cache::load(&path, true);
        assert!(inv.get(Path::new("/a"), &[stamp(10)]).is_none());
        inv.update("/b".into(), Some(vec![stamp(10)]), &[album(false)]);
        inv.save(&path).unwrap();
        // Each mode keeps its own entries.
        let loaded = Cache::load(&path, false);
        assert!(loaded.get(Path::new("/a"), &[stamp(10)]).is_some());
        assert!(loaded.get(Path::new("/b"), &[stamp(10)]).is_none());
        let loaded = Cache::load(&path, true);
        assert!(loaded.get(Path::new("/b"), &[stamp(10)]).is_some());
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }
}
