// SPDX-License-Identifier: GPL-3.0-or-later

#![forbid(unsafe_code)]

mod app;
mod cache;
mod crc;
mod db;
mod decode;
mod disc;
mod flacout;
mod parity;
mod repair;
mod report;
mod scan;
mod ui;
mod verify;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, KeyEventKind};
use ratatui::crossterm::execute;

use app::{App, Event};
use cache::Cache;
use db::Db;
use verify::Options;

/// Checks a music library's lossless CD rips against the AccurateRip database.
#[derive(Parser, Debug)]
#[command(version)]
struct Cli {
    /// Library folder to scan.
    dir: PathBuf,
    /// Minimum AccurateRip confidence for a track to count as accurate.
    /// Use 2 if you have submitted your own rips to AccurateRip.
    #[arg(long, default_value_t = 1)]
    min_confidence: u32,
    /// Ignore dBpoweramp ACCURATERIPRESULT tags and always decode.
    #[arg(long)]
    no_tag: bool,
    /// Worker threads (default: number of CPUs).
    #[arg(long)]
    threads: Option<usize>,
    /// Pressing offset search range in samples (0 disables the search).
    #[arg(long, default_value_t = crc::MAX_OFFSET, value_parser = clap::value_parser!(i64).range(0..=crc::MAX_OFFSET))]
    offset_range: i64,
    /// Don't list albums that aren't lossless 44.1 kHz/16-bit audio.
    #[arg(long)]
    skip_non_cd: bool,
    /// Report path (JSON; a .txt summary is written alongside). Written on `e`,
    /// or automatically when finished in --headless mode.
    #[arg(long, default_value = "accuraterip-report.json")]
    report: PathBuf,
    /// No TUI: print one line per album and write the report when done.
    #[arg(long)]
    headless: bool,
    /// Don't ask the CUETools database (CTDB) for the data track of Enhanced
    /// CDs that AccurateRip doesn't know by their audio tracks alone.
    #[arg(long)]
    no_ctdb: bool,
    /// Use CTDB as the primary source: albums matching CTDB's parity data are
    /// accurate, and only those that don't are checked against AccurateRip.
    #[arg(long, conflicts_with = "no_ctdb")]
    invert: bool,
    /// Repair albums with ripping errors using CTDB's repair data, writing
    /// corrected copies of the damaged tracks under this folder (mirroring
    /// the library layout). The library itself is never modified.
    #[arg(long, value_name = "DIR")]
    repair: Option<PathBuf>,
    /// Check every album against CTDB's parity data (no writing).
    #[arg(long, hide = true)]
    ctdb_check: bool,
    /// Re-verify every folder, ignoring cached results (the cache is still updated).
    #[arg(long)]
    rescan: bool,
    /// Results cache file [default: ~/.cache/accuraterip-tui/cache.json, or
    /// %LOCALAPPDATA%\accuraterip-tui\cache.json on Windows].
    #[arg(long)]
    cache: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let opts = Options {
        min_confidence: cli.min_confidence,
        use_tags: !cli.no_tag,
        offset_range: cli.offset_range,
        max_candidates: 4,
        use_ctdb: !cli.no_ctdb,
        ctdb_first: cli.invert,
    };
    let threads = cli
        .threads
        .unwrap_or_else(|| thread::available_parallelism().map_or(4, |n| n.get()))
        .max(1);

    let cache_path = cli.cache.clone().unwrap_or_else(cache::default_path);
    let cache = Cache::load(&cache_path, cli.invert);
    let prior = (!cli.rescan).then(|| Arc::new(cache.clone()));

    let repair_opts = RepairOptions {
        out: cli.repair.clone(),
        check_all: cli.ctdb_check,
    };
    let (tx, rx) = mpsc::channel();
    // Canonicalize so cache keys don't depend on how the path was typed.
    let root = std::fs::canonicalize(&cli.dir).unwrap_or_else(|_| cli.dir.clone());
    let db = Arc::new(Db::new());
    let repairer = app::Repairer {
        tx: tx.clone(),
        db: db.clone(),
        root: root.clone(),
        out: cli.repair.clone().unwrap_or_else(repair::default_out),
    };
    // Work out both modes' results (normal and --invert) so a later run in
    // either mode comes from the cache; without CTDB there's only one.
    let both = !cli.no_ctdb;
    start(root, threads, opts, both, prior, repair_opts, db, tx);

    let mut app = App::new(cli.report.clone(), cli.skip_non_cd, cache, cache_path);
    app.repairer = Some(repairer);
    if cli.headless {
        run_headless(&mut app, rx)
    } else {
        let mut terminal = ratatui::init();
        execute!(std::io::stdout(), EnableMouseCapture)?;
        let result = run_tui(&mut terminal, &mut app, rx);
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        ratatui::restore();
        app.save_cache();
        result
    }
}

/// Scans `root` and processes folders on `threads` worker threads.
#[derive(Clone)]
struct RepairOptions {
    out: Option<PathBuf>,
    check_all: bool,
}

/// Checks albums with ripping errors against CTDB's parity data (every
/// album with `check_all`), writing repaired copies when `--repair` is given.
fn maybe_repair(
    album: &mut verify::AlbumResult,
    job: &scan::AlbumJob,
    root: &std::path::Path,
    db: &Db,
    ro: &RepairOptions,
    known: Option<(repair::Outcome, String)>,
    progress: &mut dyn FnMut(f32),
) {
    // Already checked against CTDB (--invert).
    if album.repair_outcome.is_some() {
        return;
    }
    let wants = ro.check_all
        || (album.verdict == verify::Verdict::RipErrors
            && album.ctdb_known
            && album.diagnosis.is_none());
    let Some(toc) = album.ctdb_toc.clone() else {
        return;
    };
    if wants {
        let dest = ro
            .out
            .as_ref()
            .map(|o| repair::destination(o, root, &job.dir));
        let (outcome, msg) =
            known.unwrap_or_else(|| repair::run(job, &toc, db, dest.as_deref(), progress));
        if album.verdict == verify::Verdict::RipErrors {
            album.reason = format!("{}; {}", album.reason, outcome.verdict_note());
        }
        // An exact CTDB match is a final answer (until --rescan), like an
        // accurate rip; everything else is worth rechecking next run.
        if outcome == repair::Outcome::Verified {
            album.recheck = false;
        }
        album.repair_outcome = Some(outcome);
        album.repair = Some(msg);
    }
}

/// Checks one album in this run's mode and, with `both`, in the other mode
/// too, sharing the AccurateRip and CTDB checks between them. `prev` holds
/// cached results for (this mode, the other mode), which are reused.
#[allow(clippy::too_many_arguments)]
fn check_album(
    job: &scan::AlbumJob,
    root: &std::path::Path,
    db: &Db,
    opts: &Options,
    both: bool,
    ro: &RepairOptions,
    prev: (Option<verify::AlbumResult>, Option<verify::AlbumResult>),
    progress: &mut dyn FnMut(f32),
) -> (verify::AlbumResult, Option<verify::AlbumResult>) {
    let dest = ro
        .out
        .as_ref()
        .map(|o| repair::destination(o, root, &job.dir));
    if !both {
        let mut album = match opts.ctdb_first {
            true => verify::verify_ctdb_first(job, db, opts, dest.as_deref(), None, progress).0,
            false => verify::verify(job, db, opts, progress),
        };
        maybe_repair(&mut album, job, root, db, ro, None, progress);
        return (album, None);
    }
    let (normal, inverted) = match opts.ctdb_first {
        true => (prev.1, prev.0),
        false => (prev.0, prev.1),
    };
    let (normal, inverted) = match (normal, inverted) {
        (Some(n), Some(i)) => (n, i),
        (Some(normal), None) => {
            let inverted =
                verify::verify_ctdb_first(job, db, opts, dest.as_deref(), None, progress).0;
            (normal, inverted)
        }
        (None, Some(inverted)) => {
            let mut normal = verify::verify(job, db, opts, progress);
            maybe_repair(&mut normal, job, root, db, ro, None, progress);
            (normal, inverted)
        }
        (None, None) => check_both(job, root, db, opts, dest.as_deref(), ro, progress),
    };
    match opts.ctdb_first {
        true => (inverted, Some(normal)),
        false => (normal, Some(inverted)),
    }
}

/// Both modes' results for an album, from one AccurateRip and one CTDB check.
fn check_both(
    job: &scan::AlbumJob,
    root: &std::path::Path,
    db: &Db,
    opts: &Options,
    dest: Option<&std::path::Path>,
    ro: &RepairOptions,
    progress: &mut dyn FnMut(f32),
) -> (verify::AlbumResult, verify::AlbumResult) {
    let mut normal = verify::verify(job, db, opts, &mut |f| progress(f / 2.0));
    let (inverted, checked) =
        verify::verify_ctdb_first(job, db, opts, dest, Some(&normal), &mut |f| {
            progress(0.5 + f / 2.0)
        });
    // Normal mode's CTDB check is the same one if it's for the same TOC.
    let known = checked.filter(|_| inverted.ctdb_toc == normal.ctdb_toc);
    maybe_repair(&mut normal, job, root, db, ro, known, progress);
    (normal, inverted)
}

#[allow(clippy::too_many_arguments)]
fn start(
    root: PathBuf,
    threads: usize,
    opts: Options,
    both: bool,
    prior: Option<Arc<Cache>>,
    ro: RepairOptions,
    db: Arc<Db>,
    tx: Sender<Event>,
) {
    thread::spawn(move || {
        let folders = scan::find_folders(&root);
        let _ = tx.send(Event::Total(folders.len()));
        let queue = Arc::new(Mutex::new(VecDeque::from(folders)));
        for worker in 0..threads {
            let (queue, db, opts, tx) = (queue.clone(), db.clone(), opts, tx.clone());
            let prior = prior.clone();
            let ro = ro.clone();
            let root = root.clone();
            thread::spawn(move || {
                loop {
                    let Some((dir, files)) = queue.lock().unwrap().pop_front() else {
                        break;
                    };
                    let stamps = cache::stamps(&files);
                    let other_opts = Options {
                        ctdb_first: !opts.ctdb_first,
                        ..opts
                    };
                    let cached = |other: bool| {
                        let (c, s) = prior.as_ref().zip(stamps.as_ref())?;
                        let (albums, o) = match other {
                            true => (c.get_other(&dir, s)?, &other_opts),
                            false => (c.get(&dir, s)?, &opts),
                        };
                        verify::cached_valid(&albums, o).then_some(albums)
                    };
                    let hit = cached(false);
                    let other_hit = both.then(|| cached(true)).flatten();
                    // With both modes wanted, only skip folders cached in both.
                    if let Some(albums) = hit.clone().filter(|_| !both || other_hit.is_some()) {
                        let _ = tx.send(Event::Done {
                            worker,
                            dir,
                            stamps,
                            from_cache: true,
                            albums,
                            other: None,
                        });
                        continue;
                    }
                    let label = dir
                        .strip_prefix(&root)
                        .unwrap_or(&dir)
                        .display()
                        .to_string();
                    let _ = tx.send(Event::Started { worker, label });
                    let tracks = files.iter().map(|f| scan::probe_track(f)).collect();
                    let jobs = scan::group(&dir, tracks);
                    let n = jobs.len() as f32;
                    // A mode that is cached is reused album by album, if the
                    // albums still line up with this folder's.
                    let lined_up = |a: Option<Vec<verify::AlbumResult>>| {
                        a.filter(|a| {
                            a.len() == jobs.len()
                                && a.iter().zip(&jobs).all(|(r, j)| r.title == j.title)
                        })
                        .map(Vec::into_iter)
                    };
                    let (mut hit, mut other_hit) = (lined_up(hit), lined_up(other_hit));
                    let (albums, other): (Vec<_>, Vec<_>) = jobs
                        .iter()
                        .enumerate()
                        .map(|(i, job)| {
                            let mut progress = |frac: f32| {
                                let _ = tx.send(Event::Progress {
                                    worker,
                                    frac: (i as f32 + frac) / n,
                                });
                            };
                            let prev = (
                                hit.as_mut().and_then(Iterator::next),
                                other_hit.as_mut().and_then(Iterator::next),
                            );
                            check_album(job, &root, &db, &opts, both, &ro, prev, &mut progress)
                        })
                        .unzip();
                    let _ = tx.send(Event::Done {
                        worker,
                        dir,
                        stamps,
                        from_cache: false,
                        albums,
                        other: both.then(|| other.into_iter().flatten().collect()),
                    });
                }
            });
        }
    });
}

fn run_tui(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rx: Receiver<Event>,
) -> Result<()> {
    loop {
        // Progress events arrive per decoded packet; coalesce them per frame.
        while let Ok(ev) = rx.try_recv() {
            app.handle(ev);
        }
        terminal.draw(|f| ui::draw(f, app))?;
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                event::Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if app.key(key) {
                        return Ok(());
                    }
                }
                event::Event::Mouse(m) => app.mouse(m),
                _ => {}
            }
        }
    }
}

fn run_headless(app: &mut App, rx: Receiver<Event>) -> Result<()> {
    while !app.finished() {
        let Ok(ev) = rx.recv() else { break };
        if let Event::Done { albums, .. } = &ev {
            for a in albums
                .iter()
                .filter(|a| !app.skip_non_cd || !app::is_non_cd(a))
            {
                println!("{:<18} {}  [{}]", a.verdict.label(), a.title, a.reason);
                if let Some(r) = &a.repair {
                    println!("{:<18} {r}", "");
                }
            }
        }
        app.handle(ev);
    }
    app.save_cache();
    let (json, txt) = report::write(&app.report_path, &app.albums)?;
    eprintln!(
        "{} of {} folders from cache ({})",
        app.cached,
        app.done,
        app.cache_path.display()
    );
    eprintln!("Report written to {} and {}", json.display(), txt.display());
    Ok(())
}
