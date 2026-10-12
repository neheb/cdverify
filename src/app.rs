// SPDX-License-Identifier: GPL-3.0-or-later

//! TUI application state.

use std::collections::BTreeMap;
use std::path::PathBuf;

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Margin, Position, Rect};
use ratatui::widgets::ListState;

use crate::cache::{Cache, FileStamp};
use crate::report;
use crate::verify::{AlbumResult, Verdict};

pub enum Event {
    /// Number of folders to process, once scanning finishes.
    Total(usize),
    Started {
        worker: usize,
        label: String,
    },
    Progress {
        worker: usize,
        frac: f32,
    },
    /// Progress or outcome of a repair started with `r`.
    Repair {
        dir: PathBuf,
        title: String,
        msg: String,
        done: bool,
    },
    Done {
        worker: usize,
        dir: PathBuf,
        /// File stamps for the cache (`None` if a file couldn't be stat'ed).
        stamps: Option<Vec<FileStamp>>,
        from_cache: bool,
        albums: Vec<AlbumResult>,
        /// With `--both`: the other mode's results, for the cache.
        other: Option<Vec<AlbumResult>>,
    },
}

/// Screen areas from the last draw, for mouse hit-testing.
#[derive(Debug, Default)]
pub struct HitAreas {
    pub tabs: Vec<Rect>,
    pub list: Rect,
    pub details: Rect,
}

/// What the `r` key needs to repair an album in the background.
pub struct Repairer {
    pub tx: std::sync::mpsc::Sender<Event>,
    pub db: std::sync::Arc<crate::db::Db>,
    pub root: PathBuf,
    pub out: PathBuf,
}

pub struct App {
    pub albums: Vec<AlbumResult>,
    pub total: Option<usize>,
    pub done: usize,
    /// Per worker: (folder being processed, progress within it).
    pub active: BTreeMap<usize, (String, f32)>,
    pub tab: usize,
    pub lists: [ListState; 4],
    pub detail_scroll: usize,
    pub message: Option<String>,
    pub report_path: PathBuf,
    pub skip_non_cd: bool,
    pub cache: Cache,
    pub cache_path: PathBuf,
    cache_dirty: bool,
    /// Folders whose results came from the cache.
    pub cached: usize,
    pub hit: HitAreas,
    pub repairer: Option<Repairer>,
    repairing: bool,
}

impl App {
    pub fn new(report_path: PathBuf, skip_non_cd: bool, cache: Cache, cache_path: PathBuf) -> App {
        App {
            albums: Vec::new(),
            total: None,
            done: 0,
            active: BTreeMap::new(),
            tab: 0,
            lists: Default::default(),
            detail_scroll: 0,
            message: None,
            report_path,
            skip_non_cd,
            cache,
            cache_path,
            cache_dirty: false,
            cached: 0,
            hit: HitAreas::default(),
            repairer: None,
            repairing: false,
        }
    }

    pub fn finished(&self) -> bool {
        self.total.is_some_and(|t| self.done >= t)
    }

    pub fn bucket(&self, v: Verdict) -> Vec<&AlbumResult> {
        self.albums.iter().filter(|a| a.verdict == v).collect()
    }

    pub fn selected(&self) -> Option<&AlbumResult> {
        let v = Verdict::ALL[self.tab];
        let i = self.lists[self.tab].selected()?;
        self.bucket(v).get(i).copied()
    }

    pub fn handle(&mut self, ev: Event) {
        match ev {
            Event::Total(n) => self.total = Some(n),
            Event::Started { worker, label } => {
                self.active.insert(worker, (label, 0.0));
            }
            Event::Progress { worker, frac } => {
                if let Some(a) = self.active.get_mut(&worker) {
                    a.1 = frac;
                }
            }
            Event::Repair {
                dir,
                title,
                msg,
                done,
            } => {
                if done {
                    self.repairing = false;
                    if let Some(a) = self
                        .albums
                        .iter_mut()
                        .find(|a| a.dir == dir && a.title == title)
                    {
                        a.repair = Some(msg.clone());
                    }
                }
                self.message = Some(msg);
            }
            Event::Done {
                worker,
                dir,
                stamps,
                from_cache,
                albums,
                other,
            } => {
                self.active.remove(&worker);
                self.done += 1;
                if from_cache {
                    self.cached += 1;
                } else {
                    if let Some(other) = other {
                        self.cache.update_other(dir.clone(), stamps.clone(), &other);
                    }
                    self.cache.update(dir, stamps, &albums);
                    self.cache_dirty = true;
                }
                if self.finished() {
                    self.save_cache();
                }
                for a in albums {
                    if self.skip_non_cd && is_non_cd(&a) {
                        continue;
                    }
                    self.insert(a);
                }
            }
        }
    }

    /// Writes the cache if it changed since the last save.
    pub fn save_cache(&mut self) {
        if !self.cache_dirty {
            return;
        }
        match self.cache.save(&self.cache_path) {
            Ok(()) => self.cache_dirty = false,
            Err(e) => self.message = Some(format!("Saving cache failed: {e}")),
        }
    }

    /// Inserts keeping each bucket sorted, and keeps the selection on the same album.
    fn insert(&mut self, album: AlbumResult) {
        let tab = Verdict::ALL
            .iter()
            .position(|&v| v == album.verdict)
            .unwrap();
        let key = |a: &AlbumResult| (a.dir.clone(), a.title.clone());
        let pos = self.albums.partition_point(|a| key(a) < key(&album));
        let rank = self.albums[..pos]
            .iter()
            .filter(|a| a.verdict == album.verdict)
            .count();
        self.albums.insert(pos, album);
        let list = &mut self.lists[tab];
        match list.selected() {
            None => list.select(Some(0)),
            Some(s) if s >= rank => list.select(Some(s + 1)),
            _ => {}
        }
    }

    /// Returns true to quit.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        let len = self.bucket(Verdict::ALL[self.tab]).len();
        match key.code {
            KeyCode::Char('q') => return true,
            // Esc is often pressed to dismiss a terminal text selection, so it
            // only clears the status message.
            KeyCode::Esc => self.message = None,
            KeyCode::Char('c') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.copy_musicbrainz_url()
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.set_tab((self.tab + 1) % 4),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.set_tab((self.tab + 3) % 4)
            }
            KeyCode::Char(c @ '1'..='4') => self.set_tab(c as usize - '1' as usize),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Home | KeyCode::Char('g') => self.move_selection(isize::MIN),
            KeyCode::End | KeyCode::Char('G') => self.move_selection(len as isize),
            KeyCode::PageDown => self.move_selection(self.list_page()),
            KeyCode::PageUp => self.move_selection(-self.list_page()),
            KeyCode::Char('r') => self.start_repair(),
            KeyCode::Char('e') => {
                self.message = Some(match report::write(&self.report_path, &self.albums) {
                    Ok((json, txt)) => {
                        format!("Report written to {} and {}", json.display(), txt.display())
                    }
                    Err(e) => format!("Report failed: {e}"),
                });
            }
            _ => {}
        }
        false
    }

    /// Clicks select tabs and albums; the wheel moves the album selection or
    /// scrolls the track details, depending on what is under the pointer.
    pub fn mouse(&mut self, ev: MouseEvent) {
        let pos = Position::new(ev.column, ev.row);
        let list_inner = self.hit.list.inner(Margin::new(1, 1));
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(i) = self.hit.tabs.iter().position(|r| r.contains(pos)) {
                    self.set_tab(i);
                } else if list_inner.contains(pos) {
                    let row = (ev.row - list_inner.y) as usize + self.lists[self.tab].offset();
                    if row < self.bucket(Verdict::ALL[self.tab]).len() {
                        self.lists[self.tab].select(Some(row));
                        self.detail_scroll = 0;
                    }
                }
            }
            MouseEventKind::ScrollDown if self.hit.list.contains(pos) => self.move_selection(1),
            MouseEventKind::ScrollUp if self.hit.list.contains(pos) => self.move_selection(-1),
            MouseEventKind::ScrollDown if self.hit.details.contains(pos) => self.detail_scroll += 3,
            MouseEventKind::ScrollUp if self.hit.details.contains(pos) => {
                self.detail_scroll = self.detail_scroll.saturating_sub(3)
            }
            _ => {}
        }
    }

    /// Checks the selected album against CTDB's repair data in the
    /// background and writes repaired copies if possible.
    fn start_repair(&mut self) {
        if self.repairing {
            self.message = Some("A repair is already running".into());
            return;
        }
        let (Some(r), Some(album)) = (&self.repairer, self.selected()) else {
            return;
        };
        if album.repair_outcome == Some(crate::repair::Outcome::Verified) {
            self.message = Some("This rip matches CTDB exactly; there's nothing to repair".into());
            return;
        }
        let Some(toc) = album.ctdb_toc.clone() else {
            self.message = Some("This album can't be repaired (not checked against CTDB)".into());
            return;
        };
        let (dir, title) = (album.dir.clone(), album.title.clone());
        let paths: Vec<PathBuf> = album.tracks.iter().map(|t| t.path.clone()).collect();
        let (tx, db) = (r.tx.clone(), r.db.clone());
        let dest = crate::repair::destination(&r.out, &r.root, &dir);
        self.repairing = true;
        self.message = Some(format!("Checking {title} against CTDB…"));
        std::thread::spawn(move || {
            let tracks = paths.iter().map(|p| crate::scan::probe_track(p)).collect();
            let job = crate::scan::AlbumJob {
                dir: dir.clone(),
                title: title.clone(),
                tracks,
            };
            let mut last = 0;
            let mut progress = |f: f32| {
                let pct = (f * 100.0) as u32;
                if pct >= last + 2 {
                    last = pct;
                    let msg = format!("Checking {title} against CTDB: {pct}%");
                    let _ = tx.send(Event::Repair {
                        dir: dir.clone(),
                        title: title.clone(),
                        msg,
                        done: false,
                    });
                }
            };
            let (_, msg) = crate::repair::run(&job, &toc, &db, Some(&dest), &mut progress);
            let _ = tx.send(Event::Repair {
                dir,
                title,
                msg,
                done: true,
            });
        });
    }

    fn copy_musicbrainz_url(&mut self) {
        let Some(url) = self.selected().and_then(|a| a.musicbrainz_url()) else {
            self.message = Some("No MusicBrainz disc ID for this album".into());
            return;
        };
        self.message = Some(match copy_to_clipboard(&url) {
            Ok(()) => format!("Copied {url}"),
            Err(e) => format!("Copy failed: {e}"),
        });
    }

    /// Albums visible in the list (inside its border).
    fn list_page(&self) -> isize {
        (self.hit.list.height.saturating_sub(2) as isize).max(1)
    }

    fn set_tab(&mut self, tab: usize) {
        self.tab = tab;
        self.detail_scroll = 0;
    }

    /// Moves the album selection by `delta`, clamped to the list.
    fn move_selection(&mut self, delta: isize) {
        let len = self.bucket(Verdict::ALL[self.tab]).len();
        if len == 0 {
            return;
        }
        let list = &mut self.lists[self.tab];
        let cur = list.selected().unwrap_or(0) as isize;
        list.select(Some(
            cur.saturating_add(delta).clamp(0, len as isize - 1) as usize
        ));
        self.detail_scroll = 0;
    }
}

/// Copies via the Windows clipboard or `wl-copy` on Wayland, otherwise asks
/// the terminal to do it with an OSC 52 escape sequence (also works over SSH
/// in most terminals).
fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
    use std::io::Write;

    #[cfg(windows)]
    if clipboard_win::set_clipboard_string(text).is_ok() {
        return Ok(());
    }

    if std::env::var_os("WAYLAND_DISPLAY").is_some()
        && std::process::Command::new("wl-copy")
            .arg(text)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    {
        return Ok(());
    }
    let mut out = std::io::stdout();
    write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()))?;
    out.flush()
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

pub fn is_non_cd(a: &AlbumResult) -> bool {
    a.verdict == Verdict::CannotCheck
        && (a.reason.starts_with("Not CD quality")
            || a.reason == "lossy format"
            || a.reason == "unsupported format")
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_padding() {
        assert_eq!(super::base64(b"f"), "Zg==");
        assert_eq!(super::base64(b"fo"), "Zm8=");
        assert_eq!(super::base64(b"foo"), "Zm9v");
        assert_eq!(super::base64(b"foobar"), "Zm9vYmFy");
    }
}
