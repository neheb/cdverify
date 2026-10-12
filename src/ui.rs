// SPDX-License-Identifier: GPL-3.0-or-later

//! Rendering.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Gauge, List, ListItem, Paragraph, Row, Table, Tabs};

use crate::app::App;
use crate::verify::{AlbumResult, TrackStatus, Verdict};

pub fn draw(f: &mut Frame<'_>, app: &mut App) {
    let [top, tabs, main, footer] = Layout::vertical([
        Constraint::Length(3 + ACTIVE_ROWS as u16),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(f.area());

    draw_progress(f, app, top);

    let titles: Vec<Line<'_>> = Verdict::ALL
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            Line::from(vec![
                Span::raw(format!("{} ", i + 1)).dim(),
                Span::styled(v.label(), verdict_style(v)),
                Span::raw(format!(" ({})", app.bucket(v).len())),
            ])
        })
        .collect();
    app.hit.tabs = tab_areas(&titles, tabs);
    f.render_widget(
        Tabs::new(titles)
            .select(app.tab)
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        tabs,
    );

    let [left, right] =
        Layout::horizontal([Constraint::Percentage(38), Constraint::Fill(1)]).areas(main);
    let verdict = Verdict::ALL[app.tab];
    let items: Vec<ListItem<'_>> = app
        .bucket(verdict)
        .iter()
        .map(|a| ListItem::new(a.title.clone()))
        .collect();
    let list = List::new(items)
        .block(Block::bordered().title(format!(" {} ", verdict.label())))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("› ");
    f.render_stateful_widget(list, left, &mut app.lists[app.tab]);
    app.hit.list = left;
    app.hit.details = right;

    let tracks = app.selected().map_or(0, |a| a.tracks.len());
    app.detail_scroll = app.detail_scroll.min(tracks.saturating_sub(1));
    let scroll = app.detail_scroll;
    match app.selected() {
        Some(album) => draw_details(f, album, scroll, right),
        None => f.render_widget(Block::bordered().title(" Match Details "), right),
    }

    let help = "←/→ tab  ↑/↓ PgUp/PgDn select  c copy MusicBrainz URL  r repair  e export  q quit";
    let line = match &app.message {
        Some(m) => Line::from(vec![
            Span::raw(m.clone()).yellow(),
            Span::raw("  "),
            help.dim(),
        ]),
        None => Line::from(help.dim()),
    };
    f.render_widget(Paragraph::new(line), footer);
}

/// Where each tab title lands, matching `Tabs`' default layout: one space of
/// padding either side and a one-column divider between titles.
fn tab_areas(titles: &[Line<'_>], area: Rect) -> Vec<Rect> {
    let mut x = area.x;
    titles
        .iter()
        .map(|t| {
            let w = t.width() as u16 + 2;
            let r = Rect::new(x, area.y, w, 1).intersection(area);
            x = x.saturating_add(w + 1);
            r
        })
        .collect()
}

/// Folders in progress shown under the gauge; the panel keeps this height
/// even when fewer are running so the layout doesn't jump.
const ACTIVE_ROWS: usize = 4;

fn draw_progress(f: &mut Frame<'_>, app: &App, area: Rect) {
    let block = Block::bordered().title(" AccurateRip ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let rows = Layout::vertical([Constraint::Length(1); 1 + ACTIVE_ROWS]).split(inner);

    let (ratio, label) = match app.total {
        None => (0.0, "Scanning for audio files…".to_string()),
        Some(0) => (1.0, "No audio files found".to_string()),
        Some(t) => {
            let active: f32 = app.active.values().map(|a| a.1).sum();
            let r = ((app.done as f32 + active) / t as f32).clamp(0.0, 1.0);
            let state = if app.finished() { "Done" } else { "Checking" };
            (r as f64, format!("{state}: {}/{} folders", app.done, t))
        }
    };
    f.render_widget(
        Gauge::default()
            .gauge_style(Color::Cyan)
            .ratio(ratio)
            // Gauge always centres its label; padding it to the full width
            // pins the text to the left edge.
            .label(format!("{label:<w$}", w = rows[0].width as usize)),
        rows[0],
    );
    for ((name, frac), row) in app.active.values().zip(rows.iter().skip(1)) {
        let line = Line::from(vec![
            Span::raw(format!("{:>3.0}% ", frac * 100.0)).cyan(),
            Span::raw(name.clone()).dim(),
        ]);
        f.render_widget(Paragraph::new(line), *row);
    }
}

fn draw_details(f: &mut Frame<'_>, a: &AlbumResult, scroll: usize, area: Rect) {
    let block = Block::bordered().title(" Match Details ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Message lines wrap at the width of the MusicBrainz line (or the pane).
    let mb_width = a
        .musicbrainz_url()
        .map_or(71, |u| "MusicBrainz ".len() + u.len());
    let width = mb_width.min(inner.width as usize).max(20);
    let reason = match a.cached {
        true => format!("{}  (cached result)", a.reason),
        false => a.reason.clone(),
    };
    let mut info = vec![
        Line::from(a.title.clone().bold()),
        Line::from(a.dir.display().to_string().dim()),
    ];
    info.extend(wrap(&reason, verdict_style(a.verdict), width));
    if let Some(id) = &a.disc_id {
        info.push(Line::from(vec![
            "Disc ID ".dim(),
            Span::raw(id.clone()),
            format!("  ({} pressing entries)", a.pressings).dim(),
        ]));
    }
    if let Some(note) = &a.data_track {
        info.extend(wrap(note, Style::new().dim(), width));
    }
    if let Some(d) = &a.diagnosis {
        info.extend(wrap(d, Style::new().red(), width));
    }
    if let Some(note) = &a.ctdb_repair {
        info.extend(wrap(note, Style::new().cyan(), width));
    }
    if let Some(r) = &a.repair {
        info.extend(wrap(r, Style::new().cyan().bold(), width));
    }
    if let Some(url) = a.musicbrainz_url() {
        info.push(Line::from(vec!["MusicBrainz ".dim(), Span::raw(url)]));
    }
    if let Some(tag) = &a.mb_tag_mismatch {
        info.push(Line::from(
            format!("MUSICBRAINZ_DISCID tag differs: {tag}").yellow(),
        ));
    }
    let [head, table] = Layout::vertical([
        Constraint::Length(info.len() as u16 + 1),
        Constraint::Fill(1),
    ])
    .areas(inner);
    f.render_widget(Paragraph::new(info), head);

    let hex = |c: Option<u32>| c.map_or("-".into(), |c| format!("{c:08X}"));
    let rows: Vec<Row<'_>> = a
        .tracks
        .iter()
        .skip(scroll)
        .map(|t| {
            let name = t
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let status = match &t.note {
                Some(n) if t.status != TrackStatus::Accurate => {
                    format!("{} ({n})", t.status.label())
                }
                _ => t.status.label().to_string(),
            };
            Row::new(vec![
                Cell::from(format!("{:>2}", t.number)),
                Cell::from(name),
                Cell::from(hex(t.v1)),
                Cell::from(hex(t.v2)),
                Cell::from(hex(t.db_crc)),
                Cell::from(format!("{:>4}", t.confidence)),
                Cell::from(if t.offset == 0 {
                    String::new()
                } else {
                    format!("{:+}", t.offset)
                }),
                Cell::from(status).style(status_style(t.status)),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(2),
        Constraint::Fill(1),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(4),
        Constraint::Length(6),
        Constraint::Length(26),
    ];
    let header = Row::new(["#", "File", "v1", "v2", "AR", "Conf", "Offset", "Status"])
        .style(Style::new().bold().underlined());
    f.render_widget(
        Table::new(rows, widths).header(header).column_spacing(1),
        table,
    );
}

/// Word-wraps `text` to `width` columns; continuation lines are indented.
fn wrap(text: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ').filter(|w| !w.is_empty()) {
        let len = line.chars().count();
        if len > 0 && len + 1 + word.chars().count() > width {
            lines.push(Line::from(Span::styled(std::mem::take(&mut line), style)));
            line.push_str("  ");
        } else if len > 0 {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.trim().is_empty() {
        lines.push(Line::from(Span::styled(line, style)));
    }
    lines
}

fn verdict_style(v: Verdict) -> Style {
    match v {
        Verdict::Corrupted => Style::new().magenta(),
        Verdict::RipErrors => Style::new().red(),
        Verdict::Accurate => Style::new().green(),
        Verdict::CannotCheck => Style::new().yellow(),
    }
}

fn status_style(s: TrackStatus) -> Style {
    match s {
        TrackStatus::Accurate | TrackStatus::AccurateTag => Style::new().green(),
        TrackStatus::Mismatch => Style::new().red(),
        TrackStatus::Corrupted => Style::new().magenta(),
        TrackStatus::LowConfidence | TrackStatus::NotInDb => Style::new().yellow(),
        TrackStatus::Unchecked => Style::new().dim(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn wraps_at_word_boundaries() {
        let lines = wrap(
            "Matches CTDB exactly (confidence 10, offset +0): the rip is correct",
            Style::new(),
            30,
        );
        assert_eq!(
            text(&lines),
            [
                "Matches CTDB exactly",
                "  (confidence 10, offset +0):",
                "  the rip is correct"
            ]
        );
        assert!(text(&lines).iter().all(|l| l.chars().count() <= 30));
        assert_eq!(text(&wrap("short", Style::new(), 30)), ["short"]);
    }
}
