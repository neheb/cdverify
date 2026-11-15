// SPDX-License-Identifier: GPL-3.0-or-later

//! Persistent per-folder results cache.
//!
//! A folder's results are reused when its audio files are unchanged (same
//! names, sizes and modification times). Only final results are stored; see
//! [`AlbumResult::recheck`]. CTDB-first (default) and `--ar-first` runs
//! keep separate entries, so switching modes doesn't throw away the other
//! mode's results. Runs sharing a cache file merge their changes on save.

use std::collections::{HashMap, HashSet};
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
    /// Results of CTDB-first (default) runs.
    #[serde(default)]
    inverted: HashMap<PathBuf, Entry>,
    /// Which of the two this run uses.
    #[serde(skip)]
    ctdb_first: bool,
    /// Folders changed since loading or the last save, in `folders` and
    /// `inverted`: only these are written over what is on disk.
    #[serde(skip)]
    touched: [HashSet<PathBuf>; 2],
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
    /// `ctdb_first` picks the CTDB-first entries.
    pub fn load(path: &Path, ctdb_first: bool) -> Cache {
        let mut c = read(path);
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

    /// Like [`Cache::entries`], with the set to record changed folders in.
    fn entries_mut(
        &mut self,
        other: bool,
    ) -> (&mut HashMap<PathBuf, Entry>, &mut HashSet<PathBuf>) {
        let [folders, inverted] = &mut self.touched;
        match self.ctdb_first != other {
            true => (&mut self.inverted, inverted),
            false => (&mut self.folders, folders),
        }
    }

    /// Merges this run's changes into the cache on disk, under a lock so
    /// that runs sharing the file don't lose each other's results, and
    /// writes it atomically (temporary file + rename).
    pub fn save(&mut self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // The cache file itself is replaced on every save, so lock a
        // separate file. Released when `lock` is dropped.
        let lock = std::fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.with_added_extension("lock"))?;
        lock.lock()?;
        let mut merged = read(path);
        for ((ours, theirs), touched) in [
            (&self.folders, &mut merged.folders),
            (&self.inverted, &mut merged.inverted),
        ]
        .into_iter()
        .zip(&self.touched)
        {
            for dir in touched {
                match ours.get(dir) {
                    Some(e) => theirs.insert(dir.clone(), e.clone()),
                    None => theirs.remove(dir),
                };
            }
        }
        // Per process, in case another run writes without the lock (one
        // from before locking, or on a file system without locks).
        let tmp = path.with_added_extension(format!("{}.tmp", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let written = std::fs::File::options()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(&serde_json::to_vec(&merged)?)
            })
            .and_then(|()| std::fs::rename(&tmp, path));
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written?;
        self.touched = Default::default();
        Ok(())
    }

    /// Forgets folders under `root` that aren't in `seen` (deleted or moved
    /// since), in both modes. Returns whether any were.
    pub fn prune(&mut self, root: &Path, seen: &HashSet<PathBuf>) -> bool {
        let mut pruned = false;
        for (map, touched) in [&mut self.folders, &mut self.inverted]
            .into_iter()
            .zip(&mut self.touched)
        {
            map.retain(|dir, _| {
                let keep = !dir.starts_with(root) || seen.contains(dir);
                if !keep {
                    touched.insert(dir.clone());
                    pruned = true;
                }
                keep
            });
        }
        pruned
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
        let (entries, touched) = self.entries_mut(false);
        update(entries, touched, dir, files, albums);
    }

    /// Changes one cached album of `dir` in this mode's entries, forgetting
    /// the folder if the album then needs rechecking. Returns whether it was
    /// cached.
    pub fn update_album(
        &mut self,
        dir: &Path,
        title: &str,
        f: impl FnOnce(&mut AlbumResult),
    ) -> bool {
        let (entries, touched) = self.entries_mut(false);
        let Some(e) = entries.get_mut(dir) else {
            return false;
        };
        let Some(a) = e.albums.iter_mut().find(|a| a.title == title) else {
            return false;
        };
        f(a);
        if a.recheck {
            entries.remove(dir);
        }
        touched.insert(dir.to_path_buf());
        true
    }

    /// Like [`Cache::update`], for the other mode (`--both`).
    pub fn update_other(
        &mut self,
        dir: PathBuf,
        files: Option<Vec<FileStamp>>,
        albums: &[AlbumResult],
    ) {
        let (entries, touched) = self.entries_mut(true);
        update(entries, touched, dir, files, albums);
    }
}

/// The cache at `path`, or an empty one if it is missing or unreadable.
fn read(path: &Path) -> Cache {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Cache>(&b).ok())
        .filter(|c| c.version == VERSION)
        .unwrap_or_else(|| Cache {
            version: VERSION,
            ..Cache::default()
        })
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
    touched: &mut HashSet<PathBuf>,
    dir: PathBuf,
    files: Option<Vec<FileStamp>>,
    albums: &[AlbumResult],
) {
    touched.insert(dir.clone());
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
                Verdict::CannotCheck
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
            without_ctdb: false,
            offset_range: None,
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
    fn prunes_folders_gone_from_the_scanned_root() {
        let mut c = Cache::default();
        for dir in ["/music/a", "/music/b", "/other/c"] {
            c.update(dir.into(), Some(vec![stamp(10)]), &[album(false)]);
        }
        let seen = [PathBuf::from("/music/a")].into_iter().collect();
        assert!(c.prune(Path::new("/music"), &seen));
        assert!(c.get(Path::new("/music/a"), &[stamp(10)]).is_some());
        assert!(c.get(Path::new("/music/b"), &[stamp(10)]).is_none());
        assert!(c.get(Path::new("/other/c"), &[stamp(10)]).is_some());
        assert!(!c.prune(Path::new("/music"), &seen));
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
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn concurrent_runs_keep_each_others_results() {
        let dir = std::env::temp_dir().join(format!("cdverify-merge-test-{}", std::process::id()));
        let path = dir.join("cache.json");
        let mut seed = Cache::load(&path, false);
        seed.update("/lib/old".into(), Some(vec![stamp(10)]), &[album(false)]);
        seed.update("/lib/gone".into(), Some(vec![stamp(10)]), &[album(false)]);
        seed.save(&path).unwrap();
        // Two runs start from the same file.
        let mut a = Cache::load(&path, false);
        let mut b = Cache::load(&path, false);
        a.update("/lib/a".into(), Some(vec![stamp(10)]), &[album(false)]);
        let seen = ["/lib/old", "/lib/a"]
            .map(PathBuf::from)
            .into_iter()
            .collect();
        assert!(a.prune(Path::new("/lib"), &seen));
        b.update("/other/b".into(), Some(vec![stamp(10)]), &[album(false)]);
        a.save(&path).unwrap();
        b.save(&path).unwrap();
        let loaded = Cache::load(&path, false);
        for d in ["/lib/old", "/lib/a", "/other/b"] {
            assert!(loaded.get(Path::new(d), &[stamp(10)]).is_some(), "{d}");
        }
        // B's untouched copy of the pruned folder doesn't bring it back.
        assert!(loaded.get(Path::new("/lib/gone"), &[stamp(10)]).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
