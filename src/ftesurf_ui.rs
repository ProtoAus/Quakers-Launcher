//! World of Surf terminal presentation. Disk policy and launch authority stay in the bootstrap.
use crate::ui::{Header, Progress};
use anyhow::{Context, Result};
use ratatui::{
    backend::{CrosstermBackend, TestBackend},
    buffer::Buffer,
    crossterm::{
        cursor,
        event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
        execute,
        terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
    },
    layout::{Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Gauge, Paragraph, Wrap},
    Frame, Terminal,
};
use std::{
    io::{self, IsTerminal, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const NIGHT: Color = Color::Rgb(10, 16, 32);
const CYAN: Color = Color::Rgb(77, 235, 239);
const PURPLE: Color = Color::Rgb(174, 123, 255);
const WHITE: Color = Color::Rgb(228, 239, 255);
const DIM: Color = Color::Rgb(131, 153, 182);
const TRACK: Color = Color::Rgb(29, 44, 65);
const LOGO: [&str; 6] = [
    "███████╗██╗   ██╗██████╗ ███████╗",
    "██╔════╝██║   ██║██╔══██╗██╔════╝",
    "███████╗██║   ██║██████╔╝█████╗  ",
    "╚════██║██║   ██║██╔══██╗██╔══╝  ",
    "███████║╚██████╔╝██║  ██║██║     ",
    "╚══════╝ ╚═════╝ ╚═╝  ╚═╝╚═╝     ",
];
const COAST: [&str; 9] = [
    "           ·             .       *",
    "    .             ▄████▄",
    "                 ████████     ·",
    "          ________▀████▀________",
    "       __/   /   /   /   /   /  /",
    "    __/____/___/___/___/___/___/",
    " __/   /   /   /   /   /   /",
    "╱____/___/___/___/___/___/   ~ ~ ~",
    "   ~ ~ ~    ~ ~ ~    ~ ~ ~    ~ ~",
];

pub fn interactive(assume_yes: bool) -> bool {
    !assume_yes && io::stdin().is_terminal() && io::stdout().is_terminal()
}

pub fn human(bytes: u64) -> String {
    let mut value = bytes as f64;
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", units[unit])
}

fn panel(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(TRACK))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(CYAN).bold(),
        ))
}

// A slow light band moves across the letters; geometry never moves or flashes.
fn logo_line(line: &str, row: usize, tick: usize) -> Line<'static> {
    let base = if row < 3 { CYAN } else { PURPLE };
    let phase = tick % 46;
    Line::from(
        line.chars()
            .enumerate()
            .map(|(column, ch)| {
                let distance = (column + row * 2 + 46 - phase) % 46;
                let color = if distance < 3 { WHITE } else { base };
                Span::styled(ch.to_string(), Style::new().fg(color))
            })
            .collect::<Vec<_>>(),
    )
}

fn coast_line(line: &str, row: usize, tick: usize) -> Line<'static> {
    let mut chars: Vec<_> = line.chars().collect();
    if row == COAST.len() - 1 {
        let shift = (tick / 2) % chars.len();
        chars.rotate_left(shift);
    }
    let pulse = (tick % 32).min(32 - tick % 32) as u8;
    Line::from(
        chars
            .into_iter()
            .enumerate()
            .map(|(column, ch)| {
                let color = match ch {
                    '.' | '·' | '*' => {
                        if (tick / 3 + column + row * 5) % 16 < 3 {
                            WHITE
                        } else {
                            DIM
                        }
                    }
                    '█' | '▄' | '▀' => Color::Rgb(211 + pulse, 136 + pulse, 230 + pulse),
                    _ if row < 4 => PURPLE,
                    _ => CYAN,
                };
                Span::styled(ch.to_string(), Style::new().fg(color))
            })
            .collect::<Vec<_>>(),
    )
}

fn hero(f: &mut Frame, area: Rect, compact: bool, tick: usize) {
    if compact {
        f.render_widget(
            Paragraph::new(vec![
                logo_line("WORLD OF SURF", 0, tick).style(Style::new().bold()),
                Line::from("LAUNCHER").style(Style::new().fg(PURPLE)),
            ])
            .centered(),
            area,
        );
        return;
    }
    let cols =
        Layout::horizontal([Constraint::Percentage(54), Constraint::Percentage(46)]).split(area);
    let mut lines = vec![
        Line::from("W O R L D   O F").style(Style::new().fg(WHITE).bold()),
        Line::default(),
    ];
    for (i, line) in LOGO.iter().enumerate() {
        lines.push(logo_line(line, i, tick));
    }
    lines.push(Line::default());
    lines.push(Line::from("WORLD OF SURF  /  LAUNCHER").style(Style::new().fg(DIM)));
    f.render_widget(Paragraph::new(lines).centered(), cols[0]);
    let scene: Vec<_> = COAST
        .iter()
        .enumerate()
        .map(|(i, s)| coast_line(s, i, tick))
        .collect();
    f.render_widget(Paragraph::new(scene).centered(), cols[1]);
}

#[derive(Default)]
struct Rate {
    previous: u64,
    bps: f64,
}
impl Rate {
    fn sample(&mut self, received: u64, seconds: f64) {
        if seconds > 0.0 {
            let instant = received.saturating_sub(self.previous) as f64 / seconds;
            self.bps = 0.7 * self.bps + 0.3 * instant;
            self.previous = received;
        }
    }
}

fn percent(done: u64, total: u64) -> u64 {
    if total == 0 {
        0
    } else {
        (u128::from(done.min(total)) * 100 / u128::from(total)) as u64
    }
}

fn ratio(done: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (done as f64 / total as f64).clamp(0.0, 1.0)
    }
}

fn meter(f: &mut Frame, area: Rect, value: f64, label: String, color: Color) {
    f.render_widget(
        Gauge::default()
            .ratio(value)
            .label(label)
            .gauge_style(Style::new().fg(color).bg(TRACK))
            .style(Style::new().fg(WHITE).bg(NIGHT)),
        area,
    );
}

fn draw(f: &mut Frame, p: &Progress, rate: &Rate, tick: usize, preview: bool) {
    let area = f.area();
    f.render_widget(
        Block::default().style(Style::new().bg(NIGHT).fg(WHITE)),
        area,
    );
    let compact = area.width < 78 || area.height < 28;
    let roomy = area.height >= 22;
    let category_rows = p.categories.len().div_ceil(2) as u16;
    let show_categories = area.height >= 30 + category_rows;
    let rows = Layout::vertical([
        Constraint::Length(if compact { 2 } else { 10 }),
        Constraint::Length(if roomy { 2 } else { 0 }),
        Constraint::Length(if roomy { 6 } else { 4 }),
        Constraint::Length(if show_categories {
            category_rows + 2
        } else {
            0
        }),
        Constraint::Min(0),
        Constraint::Length(if roomy { 2 } else { 1 }),
    ])
    .split(area);
    hero(f, rows[0], compact, tick);
    if roomy {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(format!(
                    "RELEASE {}  ·  {}",
                    p.header.release, p.header.platform
                )),
                Line::from(format!("MIRROR {}", p.header.mirror)).style(Style::new().fg(DIM)),
            ])
            .centered(),
            rows[1],
        );
    }
    let done = p.files_done.load(Ordering::Relaxed).min(p.files_total);
    let verified = p.verified_bytes.load(Ordering::Relaxed).min(p.total_bytes);
    let box_area = rows[2];
    let block = panel("DOWNLOAD / VERIFY");
    let inner = block.inner(box_area);
    f.render_widget(block, box_area);
    let stats = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(inner);
    meter(
        f,
        stats[0],
        ratio(done as u64, p.files_total as u64),
        format!(
            "{:3}%  ·  {done}/{} files verified",
            percent(done as u64, p.files_total as u64),
            p.files_total
        ),
        CYAN,
    );
    let remaining = p.total_bytes.saturating_sub(verified);
    let eta = if remaining == 0 && done == p.files_total {
        "done".into()
    } else if rate.bps < 1.0 {
        "estimating".into()
    } else {
        let secs = (remaining as f64 / rate.bps).ceil() as u64;
        format!("{}m {:02}s", secs / 60, secs % 60)
    };
    f.render_widget(
        Paragraph::new(format!(
            "{} / {} verified",
            human(verified),
            human(p.total_bytes)
        )),
        stats[1],
    );
    f.render_widget(Paragraph::new(format!("{} /s received  ·  ETA ~{eta}\n{} Receiving / hashing  ·  installation follows verification",
        human(rate.bps as u64), ["/", "-", "\\", "|"][tick % 4])).style(Style::new().fg(DIM)), stats[2]);
    if show_categories {
        let block = panel("CONTENT / VERIFIED BYTES");
        let inner = block.inner(rows[3]);
        f.render_widget(block, rows[3]);
        let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(inner);
        for (i, c) in p.categories.iter().enumerate() {
            let cell = Rect::new(
                cols[i % 2].x,
                cols[i % 2].y + (i / 2) as u16,
                cols[i % 2].width,
                1,
            )
            .intersection(inner);
            meter(
                f,
                cell,
                ratio(c.verified.load(Ordering::Relaxed), c.total),
                format!(
                    "{} {:3}%",
                    c.name,
                    percent(c.verified.load(Ordering::Relaxed), c.total)
                ),
                PURPLE,
            );
        }
    }
    let block = panel("LIVE TRANSFERS / STAGED BYTES");
    let inner = block.inner(rows[4]);
    f.render_widget(block, rows[4]);
    for (i, slot) in p.workers.iter().enumerate() {
        let y = inner.y.saturating_add(i as u16 * 2);
        if y.saturating_add(1) >= inner.bottom() {
            break;
        }
        let active = slot.active.load(Ordering::Relaxed);
        let name = slot.name.lock().unwrap().clone();
        let cur = slot.cur.load(Ordering::Relaxed);
        let total = slot.total.load(Ordering::Relaxed);
        f.render_widget(
            Paragraph::new(format!(
                "{:02}  {}",
                i + 1,
                if active { &name } else { "waiting" }
            ))
            .style(Style::new().fg(if active { WHITE } else { DIM })),
            Rect::new(inner.x, y, inner.width, 1),
        );
        meter(
            f,
            Rect::new(inner.x, y + 1, inner.width, 1),
            if active { ratio(cur, total) } else { 0.0 },
            if active {
                format!("{} / {}", human(cur), human(total))
            } else {
                "worker idle".into()
            },
            PURPLE,
        );
    }
    f.render_widget(
        Paragraph::new(if preview {
            "PREVIEW ONLY · SAMPLE DATA · NO DOWNLOADS OR GAME LAUNCH"
        } else {
            "Resumable downloads · hash-verified files · your settings stay yours"
        })
        .style(Style::new().fg(DIM))
        .centered(),
        rows[5],
    );
}

struct Screen;
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), LeaveAlternateScreen, cursor::Show);
    }
}

pub fn start(
    progress: Arc<Progress>,
) -> Option<(Arc<AtomicBool>, Arc<AtomicBool>, JoinHandle<()>)> {
    if !interactive(false) {
        return None;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancel = cancelled.clone();
    let handle = thread::spawn(move || {
        // Raw input keeps terminal Ctrl-C in normal control flow, allowing cleanup.
        if terminal::enable_raw_mode().is_err() {
            return;
        }
        let _raw = RawMode;
        let _screen = Screen;
        if execute!(io::stdout(), EnterAlternateScreen, cursor::Hide).is_err() {
            return;
        }
        let Ok(mut term) = Terminal::new(CrosstermBackend::new(io::stdout())) else {
            return;
        };
        let mut rate = Rate::default();
        let mut last = Instant::now();
        let mut tick = 0;
        loop {
            let now = Instant::now();
            rate.sample(
                progress.done_bytes.load(Ordering::Relaxed),
                now.duration_since(last).as_secs_f64(),
            );
            last = now;
            if term
                .draw(|f| draw(f, &progress, &rate, tick, false))
                .is_err()
            {
                break;
            }
            for _ in 0..32 {
                match event::poll(Duration::ZERO) {
                    Ok(false) => break,
                    Ok(true) => match event::read() {
                        Ok(Event::Key(key)) if decision(key) == Some(false) => {
                            cancel.store(true, Ordering::Relaxed);
                            return;
                        }
                        Ok(_) => (),
                        Err(_) => {
                            cancel.store(true, Ordering::Relaxed);
                            return;
                        }
                    },
                    Err(_) => {
                        cancel.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            }
            if stopped.load(Ordering::Relaxed) {
                break;
            }
            tick += 1;
            thread::sleep(Duration::from_millis(160));
        }
    });
    Some((stop, cancelled, handle))
}

fn decision(key: KeyEvent) -> Option<bool> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    match key.code {
        KeyCode::Enter | KeyCode::Char('y' | 'Y') => Some(true),
        KeyCode::Esc | KeyCode::Char('n' | 'N' | 'q' | 'Q') => Some(false),
        KeyCode::Char('c' | 'C') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(false),
        _ => None,
    }
}
struct RawMode;
impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

fn draw_prompt(f: &mut Frame, question: &str, tick: usize) {
    let area = f.area();
    f.render_widget(
        Block::default().style(Style::new().bg(NIGHT).fg(WHITE)),
        area,
    );
    let compact = area.width < 78 || area.height < 22;
    let rows = Layout::vertical([
        Constraint::Length(if compact { 2 } else { 10 }),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .split(area);
    hero(f, rows[0], compact, tick);
    f.render_widget(
        Paragraph::new(question)
            .wrap(Wrap { trim: false })
            .centered()
            .block(panel("WORLD OF SURF / CONFIRM")),
        rows[2],
    );
    f.render_widget(
        Paragraph::new("[ ENTER / Y ] Continue     [ ESC / N ] Cancel")
            .style(Style::new().fg(CYAN))
            .centered(),
        rows[3],
    );
}

fn animated_keys(mut render: impl FnMut(&mut Frame, usize)) -> Result<bool> {
    terminal::enable_raw_mode().context("cannot read confirmation keys")?;
    let _raw = RawMode;
    let _screen = Screen;
    execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let start = Instant::now();
    loop {
        let tick = (start.elapsed().as_millis() / 160) as usize;
        term.draw(|f| render(f, tick))?;
        if event::poll(Duration::from_millis(160)).context("confirmation input failed")? {
            if let Event::Key(key) = event::read().context("confirmation input failed")? {
                if let Some(accept) = decision(key) {
                    return Ok(accept);
                }
            }
        }
    }
}

/// Noninteractive automation skips prompts; a broken interactive input never accepts.
pub fn confirm(question: &str, assume_yes: bool) -> Result<bool> {
    if !interactive(assume_yes) {
        return Ok(true);
    }
    animated_keys(|f, tick| draw_prompt(f, question, tick))
}

/// Live animation preview only, with sample progress and no install/network/game work.
pub fn demo() -> Result<()> {
    anyhow::ensure!(
        interactive(false),
        "--ui-demo needs an interactive terminal; use --ui-preview for plain text"
    );
    let p = sample();
    let rate = Rate {
        bps: 6.2 * 1048576.0,
        previous: 0,
    };
    animated_keys(|f, tick| {
        draw(f, &p, &rate, tick, true);
        let area = f.area();
        if area.height > 0 {
            f.render_widget(
                Paragraph::new("[ ENTER / ESC ] Close animation preview")
                    .style(Style::new().fg(CYAN))
                    .centered(),
                Rect::new(area.x, area.bottom() - 1, area.width, 1),
            );
        }
    })?;
    Ok(())
}

pub fn welcome() -> Result<()> {
    if !io::stdout().is_terminal() {
        println!("World of Surf Launcher");
        return Ok(());
    }
    let _ = ratatui::crossterm::ansi_support::supports_ansi();
    let width = terminal::size().map(|s| s.0).unwrap_or(80);
    let compact = width < 78;
    let mut term = Terminal::new(TestBackend::new(width, if compact { 2 } else { 10 }))?;
    term.draw(|f| {
        f.render_widget(
            Block::default().style(Style::new().fg(WHITE).bg(NIGHT)),
            f.area(),
        );
        hero(f, f.area(), compact, 0);
    })?;
    let mut out = io::stdout().lock();
    for y in 0..term.backend().buffer().area.height {
        for x in 0..width {
            let cell = &term.backend().buffer()[(x, y)];
            execute!(
                out,
                ratatui::crossterm::style::SetForegroundColor(cell.fg.into())
            )?;
            write!(out, "{}", cell.symbol())?;
        }
        execute!(out, ratatui::crossterm::style::ResetColor)?;
        writeln!(out)?;
    }
    Ok(())
}

fn sample() -> Arc<Progress> {
    let mib = 1024 * 1024;
    let p = Progress::new(
        240 * mib,
        120,
        3,
        vec![
            ("Maps", 160 * mib),
            ("Textures", 60 * mib),
            ("Game data", 20 * mib),
        ],
        Header {
            system: "Preview".into(),
            platform: "win64 / linux64".into(),
            release: "0.1.25 · sample".into(),
            mirror: "dl.proto.bar".into(),
        },
    );
    p.files_done.store(60, Ordering::Relaxed);
    p.verified_bytes.store(120 * mib, Ordering::Relaxed);
    p.done_bytes.store(132 * mib, Ordering::Relaxed);
    for c in &p.categories {
        c.verified.store(c.total / 2, Ordering::Relaxed);
    }
    for (i, name) in ["surf_horizon.bsp", "coast_materials.pk3", "csprogs.dat"]
        .iter()
        .enumerate()
    {
        p.worker_start(i, name, 8 * mib);
        p.worker_set_cur(i, (i as u64 + 1) * 2 * mib);
    }
    p
}

fn render_sample(width: u16, height: u16) -> Buffer {
    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    term.draw(|f| {
        draw(
            f,
            &sample(),
            &Rate {
                bps: 6.2 * 1048576.0,
                previous: 0,
            },
            0,
            true,
        )
    })
    .unwrap();
    term.backend().buffer().clone()
}
fn text(buffer: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// Sample frame only: safe before any install lock, network request or recovery.
pub fn preview() -> String {
    text(&render_sample(100, 32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save_snapshot(name: &str, buffer: &Buffer) {
        if let Some(root) = std::env::var_os("FTESURF_UI_SNAPSHOT_ROOT") {
            let cells: Vec<_> = buffer
                .content
                .iter()
                .map(|c| {
                    let rgb = |color| match color {
                        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
                        _ => "#0a1020".into(),
                    };
                    serde_json::json!({"text": c.symbol(), "fg": rgb(c.fg), "bg": rgb(c.bg)})
                })
                .collect();
            std::fs::write(
                std::path::Path::new(&root).join(name),
                serde_json::to_vec(&serde_json::json!({"width": buffer.area.width,
                    "height": buffer.area.height, "cells": cells}))
                .unwrap(),
            )
            .unwrap();
        }
    }

    #[test]
    fn artwork_and_progress_are_inked() {
        let b = render_sample(100, 32);
        let s = text(&b);
        for want in [
            "WORLD OF SURF",
            "50%",
            "60/120 files verified",
            "LIVE TRANSFERS",
            "surf_horizon.bsp",
            "PREVIEW ONLY",
        ] {
            assert!(s.contains(want), "missing {want}\n{s}");
        }
        assert!(b.content.iter().any(|c| c.fg == CYAN));
        assert!(b.content.iter().any(|c| c.fg == PURPLE));
        save_snapshot("preview-cells.json", &b);
    }
    #[test]
    fn compact_and_tiny_terminals_do_not_panic() {
        for (w, h) in [(60, 24), (40, 12), (20, 8), (1, 1), (0, 0)] {
            let b = render_sample(w, h);
            if w >= 40 {
                assert!(text(&b).contains("WORLD OF SURF"));
            }
        }
    }
    #[test]
    fn retransmitted_bytes_never_claim_verified_completion() {
        let p = sample();
        p.done_bytes.store(p.total_bytes * 4, Ordering::Relaxed);
        let mut term = Terminal::new(TestBackend::new(100, 38)).unwrap();
        term.draw(|f| draw(f, &p, &Rate::default(), 1, false))
            .unwrap();
        let s = text(term.backend().buffer());
        assert!(s.contains("50%"));
        assert!(!s.contains("100%"));
    }
    #[test]
    fn zero_half_and_complete_have_distinct_real_cells() {
        let p = sample();
        let mut frames = Vec::new();
        for n in [0, 60, 120] {
            p.files_done.store(n, Ordering::Relaxed);
            p.verified_bytes
                .store(p.total_bytes * n as u64 / 120, Ordering::Relaxed);
            let mut t = Terminal::new(TestBackend::new(100, 38)).unwrap();
            t.draw(|f| draw(f, &p, &Rate::default(), 0, false)).unwrap();
            let b = t.backend().buffer().clone();
            assert!(text(&b).contains(&format!("{n}/120 files verified")));
            frames.push(b);
        }
        assert_ne!(frames[0], frames[1]);
        assert_ne!(frames[1], frames[2]);
        assert!(text(&frames[2]).contains("100%"));
    }
    #[test]
    fn confirmation_is_explicit_and_key_releases_are_ignored() {
        assert_eq!(
            decision(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(true)
        );
        for key in [KeyCode::Esc, KeyCode::Char('n'), KeyCode::Char('Q')] {
            assert_eq!(
                decision(KeyEvent::new(key, KeyModifiers::NONE)),
                Some(false)
            );
        }
        assert_eq!(
            decision(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(false)
        );
        for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
            assert_eq!(
                decision(KeyEvent::new_with_kind(
                    KeyCode::Enter,
                    KeyModifiers::NONE,
                    kind
                )),
                None
            );
        }
        assert!(confirm("must not block", true).unwrap());
    }
    #[test]
    fn artwork_animates_but_progress_does_not_invent_work() {
        let p = sample();
        let rate = Rate {
            bps: 6.2 * 1048576.0,
            previous: 0,
        };
        let mut previous_art = None;
        let mut previous_progress = None;
        for tick in 0..46 {
            let mut t = Terminal::new(TestBackend::new(100, 32)).unwrap();
            t.draw(|f| draw(f, &p, &rate, tick, true)).unwrap();
            let b = t.backend().buffer();
            let art = &b.content[..1000];
            if let Some(previous) = &previous_art {
                assert_ne!(art, previous);
            }
            previous_art = Some(art.to_vec());
            let meters = &b.content[1300..1600];
            if let Some(previous) = &previous_progress {
                assert_eq!(meters, previous);
            }
            previous_progress = Some(meters.to_vec());
            assert_eq!(p.files_done.load(Ordering::Relaxed), 60);
            assert_eq!(p.verified_bytes.load(Ordering::Relaxed), 120 * 1048576);
            save_snapshot(&format!("animation-cells-{tick:02}.json"), b);
        }
    }

    #[test]
    fn tagline_is_absent_and_animated_prompts_resize_safely() {
        for (w, h) in [(100, 32), (80, 30), (60, 24), (20, 8), (1, 1), (0, 0)] {
            assert!(!text(&render_sample(w, h))
                .to_lowercase()
                .contains("ride the horizon"));
            for tick in [0, 15, 45] {
                let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
                t.draw(|f| draw_prompt(f, "Press Enter to launch.", tick))
                    .unwrap();
                let s = text(t.backend().buffer());
                assert!(!s.to_lowercase().contains("ride the horizon"));
                if w >= 60 && h >= 24 {
                    assert!(s.contains("WORLD OF SURF"));
                    assert!(s.contains("ENTER / Y"));
                }
            }
        }
    }

    #[test]
    fn all_categories_remain_visible_when_the_terminal_has_room() {
        let p = Progress::new(
            800,
            8,
            3,
            vec![
                ("Engine", 100),
                ("Plugins", 100),
                ("Game data", 100),
                ("Textures", 100),
                ("Sounds", 100),
                ("Models", 100),
                ("Maps", 100),
                ("Other", 100),
            ],
            Header::default(),
        );
        let mut t = Terminal::new(TestBackend::new(100, 38)).unwrap();
        t.draw(|f| draw(f, &p, &Rate::default(), 0, false)).unwrap();
        let s = text(t.backend().buffer());
        for c in &p.categories {
            assert!(s.contains(c.name), "missing {}", c.name);
        }
    }

    #[test]
    fn incomplete_verification_never_rounds_up_to_100_percent() {
        let p = Progress::new(1000, 300, 1, vec![("Maps", 1000)], Header::default());
        p.files_done.store(299, Ordering::Relaxed);
        p.verified_bytes.store(999, Ordering::Relaxed);
        p.categories[0].verified.store(999, Ordering::Relaxed);
        let mut t = Terminal::new(TestBackend::new(100, 32)).unwrap();
        t.draw(|f| draw(f, &p, &Rate::default(), 0, false)).unwrap();
        let s = text(t.backend().buffer());
        assert!(s.contains("299/300 files verified"));
        assert!(s.contains("99%"));
        assert!(!s.contains("100%"));
        assert_eq!(percent(300, 300), 100);
        assert_eq!(percent(u64::MAX - 1, u64::MAX), 99);
        assert_eq!(percent(u64::MAX, u64::MAX), 100);
    }

    #[test]
    fn received_rate_is_not_verified_rate() {
        let mut r = Rate::default();
        r.sample(1000, 1.0);
        assert_eq!(r.bps, 300.0);
        r.sample(1000, 1.0);
        assert!(r.bps < 300.0);
        assert_eq!(ratio(2, 1), 1.0);
        assert_eq!(ratio(0, 0), 0.0);
    }
}
