//! One black, logo-only terminal session across consent, transfers and launch.
use crate::ui::{Header, Progress};
use anyhow::{ensure, Context, Result};
use ratatui::{
    backend::{CrosstermBackend, TestBackend},
    buffer::Buffer,
    crossterm::{
        cursor,
        event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
        execute,
        terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
    },
    layout::Rect,
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, LineGauge, Paragraph, Wrap},
    Frame, Terminal,
};
use std::{
    io::{self, IsTerminal},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const BLACK: Color = Color::Black;
const CYAN: Color = Color::Rgb(77, 235, 239);
const PURPLE: Color = Color::Rgb(174, 123, 255);
const WHITE: Color = Color::Rgb(228, 239, 255);
const DIM: Color = Color::Rgb(131, 153, 182);
const LOGO: [&str; 6] = [
    "███████╗██╗   ██╗██████╗ ███████╗",
    "██╔════╝██║   ██║██╔══██╗██╔════╝",
    "███████╗██║   ██║██████╔╝█████╗  ",
    "╚════██║██║   ██║██╔══██╗██╔══╝  ",
    "███████║╚██████╔╝██║  ██║██║     ",
    "╚══════╝ ╚═════╝ ╚═╝  ╚═╝╚═╝     ",
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

fn full_logo(area: Rect) -> bool {
    // The wordmark is 32 cells, not the old 78-column dashboard width.
    area.width >= 34 && area.height >= 18
}

fn hero(f: &mut Frame, area: Rect, compact: bool, tick: usize) {
    let mut lines = if compact {
        vec![
            logo_line("WORLD OF SURF", 0, tick).bold(),
            Line::from("LAUNCHER").fg(PURPLE),
        ]
    } else {
        let mut lines = vec![
            Line::from("W O R L D   O F").fg(WHITE).bold(),
            Line::default(),
        ];
        lines.extend(
            LOGO.iter()
                .enumerate()
                .map(|(i, line)| logo_line(line, i, tick)),
        );
        lines.push(Line::default());
        lines.push(Line::from("WORLD OF SURF  /  LAUNCHER").fg(DIM));
        lines
    };
    for line in &mut lines {
        line.style = line.style.bg(BLACK);
    }
    f.render_widget(Paragraph::new(lines).centered(), area);
}

fn row(body: Rect, offset: u16, height: u16) -> Rect {
    Rect::new(body.x, body.y.saturating_add(offset), body.width, height).intersection(body)
}

fn base(f: &mut Frame, tick: usize, hint: &str) -> Rect {
    let area = f.area();
    f.render_widget(
        Block::default().style(Style::new().bg(BLACK).fg(WHITE)),
        area,
    );
    let compact = !full_logo(area);
    let top = u16::from(area.height >= 30);
    let header = if compact { 2 } else { 10 };
    hero(
        f,
        Rect::new(area.x, area.y + top, area.width, header).intersection(area),
        compact,
        tick,
    );
    let y = (top + header + 1).min(area.height);
    let footer = u16::from(area.height >= 18);
    let width = area.width.saturating_sub(4).min(68);
    let body = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + y,
        width,
        area.height.saturating_sub(y + footer),
    );
    if footer != 0 {
        f.render_widget(
            Paragraph::new(hint).fg(DIM).centered(),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
    body
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
        LineGauge::default()
            .ratio(value)
            .label(label)
            .line_set(ratatui::symbols::line::THICK)
            .filled_style(Style::new().fg(color).bg(BLACK))
            .unfilled_style(Style::new().fg(DIM).bg(BLACK))
            .style(Style::new().fg(WHITE).bg(BLACK)),
        area,
    );
}
fn draw_download(f: &mut Frame, p: &Progress, rate: &Rate, tick: usize, preview: bool) {
    let body = base(
        f,
        tick,
        if preview {
            if f.area().width < 32 {
                "PREVIEW"
            } else {
                "PREVIEW ONLY · SAMPLE DATA"
            }
        } else if f.area().width < 56 {
            "[ Esc ] Cancel"
        } else {
            "[ Esc ] Cancel · Downloaded = this session"
        },
    );
    f.render_widget(
        Paragraph::new("Downloading World of Surf")
            .fg(WHITE)
            .centered(),
        row(body, 0, 1),
    );
    // Network bytes this session are exact, including retries, not an optimistic
    // sum of live slots and verified bytes (which can double-count a finishing slot).
    let total = format!("Downloaded {}", human(p.done_bytes.load(Ordering::Relaxed)));
    let total = if body.width >= 48 {
        format!("{total}  ·  {} /s", human(rate.bps as u64))
    } else {
        total
    };
    f.render_widget(Paragraph::new(total).fg(DIM).centered(), row(body, 1, 1));
    let done = p.files_done.load(Ordering::Relaxed).min(p.files_total);
    meter(
        f,
        row(body, 2, 1),
        ratio(done as u64, p.files_total as u64),
        format!("{}% verified", percent(done as u64, p.files_total as u64)),
        CYAN,
    );
    let verified = format!("{done}/{} files verified", p.files_total);
    let verified = if body.width >= 56 {
        format!("{verified}  ·  {} total", human(p.total_bytes))
    } else {
        verified
    };
    f.render_widget(Paragraph::new(verified).fg(DIM).centered(), row(body, 3, 1));
    let start = 4 + u16::from(body.height >= 14);
    let available = body.height.saturating_sub(start);
    let two_rows = available >= p.workers.len() as u16 * 2;
    for (i, slot) in p.workers.iter().enumerate() {
        let y = start + i as u16 * if two_rows { 2 } else { 1 };
        if y >= body.height {
            break;
        }
        let active = slot.active.load(Ordering::Relaxed);
        let name = slot.name.lock().unwrap().clone();
        let cur = slot.cur.load(Ordering::Relaxed);
        let total = slot.total.load(Ordering::Relaxed);
        let label = if active {
            format!(
                "{} / {}{}",
                human(cur.min(total)),
                human(total),
                if cur >= total { " · checking" } else { "" }
            )
        } else {
            "waiting".into()
        };
        let filename = if active { name.as_str() } else { "waiting" };
        if two_rows {
            f.render_widget(
                Paragraph::new(format!("{:02}  {filename}", i + 1)).fg(if active {
                    WHITE
                } else {
                    DIM
                }),
                row(body, y, 1),
            );
            meter(
                f,
                row(body, y + 1, 1),
                if active { ratio(cur, total) } else { 0.0 },
                label,
                PURPLE,
            );
        } else {
            f.render_widget(
                Paragraph::new(format!("{:02}  {filename}  {label}", i + 1)).fg(if active {
                    WHITE
                } else {
                    DIM
                }),
                row(body, y, 1),
            );
        }
    }
}

fn draw_status(f: &mut Frame, title: &str, detail: &str, tick: usize) {
    draw_notice(f, title, detail, tick, "");
}
fn draw_notice(f: &mut Frame, title: &str, detail: &str, tick: usize, hint: &str) {
    let body = base(f, tick, hint);
    f.render_widget(
        Paragraph::new(title).centered().wrap(Wrap { trim: false }),
        row(body, 0, 2),
    );
    f.render_widget(
        Paragraph::new(detail)
            .fg(DIM)
            .centered()
            .wrap(Wrap { trim: false }),
        row(body, 3, body.height.saturating_sub(3).min(8)),
    );
}
fn draw_choice(f: &mut Frame, question: &str, detail: &str, yes: bool, tick: usize) {
    let hint = if f.area().width < 50 {
        "[ ↑↓ ] Choose  [ Enter ] Confirm"
    } else {
        "[ ↑ / ↓ ] Choose   [ Enter ] Confirm   [ Esc ] No"
    };
    let body = base(f, tick, hint);
    let question_rows = if body.width == 0 {
        1
    } else {
        (question.chars().count().div_ceil(body.width as usize) as u16).clamp(1, 2)
    };
    let needed = if detail.is_empty() || body.width == 0 {
        0
    } else {
        detail
            .lines()
            .map(|line| line.chars().count().div_ceil(body.width as usize) as u16)
            .sum()
    };
    let detail_rows = needed.min(body.height.saturating_sub(question_rows + 3));
    f.render_widget(
        Paragraph::new(question)
            .centered()
            .wrap(Wrap { trim: false }),
        row(body, 0, question_rows),
    );
    if detail_rows > 0 {
        f.render_widget(
            Paragraph::new(detail)
                .fg(DIM)
                .centered()
                .wrap(Wrap { trim: false }),
            row(body, question_rows, detail_rows),
        );
    }
    let y = question_rows + detail_rows + u16::from(body.height > question_rows + detail_rows + 2);
    for (offset, selected, label) in [(0, yes, "Yes"), (1, !yes, "No")] {
        f.render_widget(
            Paragraph::new(format!("{} {label}", if selected { "›" } else { " " }))
                .fg(if selected { CYAN } else { DIM })
                .centered(),
            row(body, y + offset, 1),
        );
    }
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
fn choice_key(key: KeyEvent, yes: &mut bool) -> Option<bool> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    match key.code {
        KeyCode::Up => {
            *yes = true;
            None
        }
        KeyCode::Down => {
            *yes = false;
            None
        }
        KeyCode::Enter => Some(*yes),
        _ => decision(key),
    }
}

#[derive(Clone)]
enum Page {
    Status(String, String),
    Choice {
        question: String,
        detail: String,
        yes: bool,
        reply: mpsc::Sender<bool>,
    },
    Download(Arc<Progress>, Arc<AtomicBool>),
    Dismiss {
        title: String,
        detail: String,
        reply: mpsc::Sender<bool>,
    },
    Preview(Arc<Progress>),
}
struct State {
    page: Mutex<Page>,
    stop: AtomicBool,
    quit: AtomicBool,
    error: Mutex<Option<String>>,
}
static ACTIVE: Mutex<Option<Arc<State>>> = Mutex::new(None);
fn active() -> Option<Arc<State>> {
    ACTIVE.lock().unwrap().clone()
}
struct Screen;
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            cursor::Show,
            ratatui::crossterm::style::ResetColor
        );
    }
}
struct RawMode;
impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

fn render_loop(state: &State, ready: mpsc::Sender<Result<()>>) {
    let result = (|| -> Result<()> {
        terminal::enable_raw_mode().context("cannot read launcher keys")?;
        let _raw = RawMode;
        let _screen = Screen;
        execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
        let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        let mut rate = Rate::default();
        let mut last = Instant::now();
        let start = last;
        ready.send(Ok(())).ok();
        while !state.stop.load(Ordering::Relaxed) {
            let page = state.page.lock().unwrap().clone();
            let now = Instant::now();
            if let Page::Download(p, _) = &page {
                rate.sample(
                    p.done_bytes.load(Ordering::Relaxed),
                    now.duration_since(last).as_secs_f64(),
                );
            }
            last = now;
            let tick = (start.elapsed().as_millis() / 160) as usize;
            term.draw(|f| match &page {
                Page::Status(title, detail) => draw_status(f, title, detail, tick),
                Page::Choice {
                    question,
                    detail,
                    yes,
                    ..
                } => draw_choice(f, question, detail, *yes, tick),
                Page::Download(p, _) => draw_download(f, p, &rate, tick, false),
                Page::Dismiss { title, detail, .. } => {
                    draw_notice(f, title, detail, tick, "[ Enter / Esc ] Close")
                }
                Page::Preview(p) => draw_download(
                    f,
                    p,
                    &Rate {
                        previous: 0,
                        bps: 6.2 * 1048576.0,
                    },
                    tick,
                    true,
                ),
            })?;
            if !event::poll(Duration::from_millis(160))? {
                continue;
            }
            if let Event::Key(key) = event::read()? {
                let mut page = state.page.lock().unwrap();
                match &mut *page {
                    Page::Choice { yes, reply, .. } => {
                        if let Some(answer) = choice_key(key, yes) {
                            reply.send(answer).ok();
                            *page = Page::Status(
                                if answer { "Preparing…" } else { "Cancelled" }.into(),
                                String::new(),
                            );
                        }
                    }
                    Page::Dismiss { reply, .. } if decision(key).is_some() => {
                        reply.send(true).ok();
                        *page = Page::Status("Closing…".into(), String::new());
                    }
                    Page::Download(_, cancel) if decision(key) == Some(false) => {
                        cancel.store(true, Ordering::Relaxed);
                        *page = Page::Status(
                            "Cancelling download…".into(),
                            "Partial files are kept for resuming.".into(),
                        );
                    }
                    Page::Preview(_) if decision(key).is_some() => {
                        state.quit.store(true, Ordering::Relaxed);
                    }
                    Page::Status(_, _) if decision(key) == Some(false) => {
                        state.quit.store(true, Ordering::Relaxed);
                    }
                    _ => (),
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        let text = format!("{error:#}");
        ready.send(Err(anyhow::anyhow!(text.clone()))).ok();
        *state.error.lock().unwrap() = Some(text);
        match &*state.page.lock().unwrap() {
            Page::Choice { reply, .. } | Page::Dismiss { reply, .. } => {
                reply.send(false).ok();
            }
            Page::Download(_, cancel) => {
                cancel.store(true, Ordering::Relaxed);
            }
            _ => (),
        }
        state.stop.store(true, Ordering::Relaxed);
    }
}

/// Owns the alternate screen for the complete interactive workflow, not a phase.
pub struct Session {
    state: Arc<State>,
    handle: Option<JoinHandle<()>>,
}
impl Session {
    pub fn begin() -> Result<Option<Self>> {
        if !interactive(false) {
            return Ok(None);
        }
        let mut current = ACTIVE.lock().unwrap();
        ensure!(current.is_none(), "launcher screen is already active");
        let state = Arc::new(State {
            page: Mutex::new(Page::Status(
                "Preparing World of Surf…".into(),
                String::new(),
            )),
            stop: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            error: Mutex::new(None),
        });
        let (ready, rx) = mpsc::channel();
        let render = state.clone();
        let handle = thread::spawn(move || render_loop(&render, ready));
        if let Err(error) = rx.recv().context("launcher screen did not start")? {
            handle.join().ok();
            return Err(error);
        }
        *current = Some(state.clone());
        Ok(Some(Self {
            state,
            handle: Some(handle),
        }))
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let mut current = ACTIVE.lock().unwrap();
        if current
            .as_ref()
            .is_some_and(|s| Arc::ptr_eq(s, &self.state))
        {
            *current = None;
        }
    }
}

pub fn message(title: &str, detail: &str) -> Result<()> {
    if let Some(state) = active() {
        ensure!(!state.quit.load(Ordering::Relaxed), "launcher cancelled");
        ensure!(
            state.error.lock().unwrap().is_none(),
            "launcher display failed"
        );
        *state.page.lock().unwrap() = Page::Status(title.into(), detail.into());
    } else if detail.is_empty() {
        println!("{title}");
    } else {
        println!("{title}\n{detail}");
    }
    Ok(())
}
pub fn welcome() -> Result<()> {
    message("World of Surf Launcher", "")
}

/// Up/Down chooses Yes/No; Enter confirms the selection. Automation never waits.
pub fn choose(question: &str, detail: &str, assume_yes: bool) -> Result<bool> {
    if !interactive(assume_yes) {
        return Ok(true);
    }
    let own = if active().is_none() {
        Session::begin()?
    } else {
        None
    };
    let state = active().context("launcher screen is unavailable")?;
    if state.quit.load(Ordering::Relaxed) {
        return Ok(false);
    }
    let (reply, rx) = mpsc::channel();
    {
        let mut page = state.page.lock().unwrap();
        ensure!(
            !state.stop.load(Ordering::Relaxed) && state.error.lock().unwrap().is_none(),
            "confirmation display failed"
        );
        *page = Page::Choice {
            question: question.into(),
            detail: detail.into(),
            yes: true,
            reply,
        };
    }
    let answer = rx.recv().context("confirmation input failed")?;
    drop(own);
    ensure!(
        state.error.lock().unwrap().is_none(),
        "confirmation display failed"
    );
    Ok(answer)
}
pub fn confirm(question: &str, assume_yes: bool) -> Result<bool> {
    choose(question, "", assume_yes)
}

pub fn dismiss(title: &str, detail: &str) -> Result<()> {
    let state = active().context("launcher screen is unavailable")?;
    let (reply, rx) = mpsc::channel();
    {
        let mut page = state.page.lock().unwrap();
        if state.quit.load(Ordering::Relaxed)
            || state.stop.load(Ordering::Relaxed)
            || state.error.lock().unwrap().is_some()
        {
            return Ok(());
        }
        *page = Page::Dismiss {
            title: title.into(),
            detail: detail.into(),
            reply,
        };
    }
    rx.recv().context("close input failed")?;
    Ok(())
}

/// The transport's stop/join contract stays unchanged; it borrows the main screen.
pub fn start(
    progress: Arc<Progress>,
) -> Option<(Arc<AtomicBool>, Arc<AtomicBool>, JoinHandle<()>)> {
    if !interactive(false) {
        return None;
    }
    let own = if active().is_none() {
        Session::begin().ok()?
    } else {
        None
    };
    let state = active()?;
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancel = cancelled.clone();
    *state.page.lock().unwrap() = Page::Download(progress, cancel.clone());
    let handle = thread::spawn(move || {
        while !stopped.load(Ordering::Relaxed) {
            if state.stop.load(Ordering::Relaxed) || state.quit.load(Ordering::Relaxed) {
                cancel.store(true, Ordering::Relaxed);
            }
            // The caller stops us only after its workers have drained.
            thread::sleep(Duration::from_millis(20));
        }
        drop(own);
    });
    Some((stop, cancelled, handle))
}

pub fn demo() -> Result<()> {
    ensure!(
        interactive(false),
        "--ui-demo needs an interactive terminal; use --ui-preview for plain text"
    );
    let session = Session::begin()?.context("interactive terminal required")?;
    *session.state.page.lock().unwrap() = Page::Preview(sample());
    while !session.state.quit.load(Ordering::Relaxed) && !session.state.stop.load(Ordering::Relaxed)
    {
        thread::sleep(Duration::from_millis(20));
    }
    ensure!(
        session.state.error.lock().unwrap().is_none(),
        "preview display failed"
    );
    Ok(())
}

fn sample() -> Arc<Progress> {
    let mib = 1024 * 1024;
    let p = Progress::new(
        60 * mib,
        120,
        4,
        vec![("Game data", 60 * mib)],
        Header::default(),
    );
    p.files_done.store(60, Ordering::Relaxed);
    p.verified_bytes.store(30 * mib, Ordering::Relaxed);
    p.done_bytes.store(36 * mib, Ordering::Relaxed);
    for (i, name) in [
        "surf_horizon.bsp",
        "coast_materials.pk3",
        "csprogs.dat",
        "menu.dat",
    ]
    .iter()
    .enumerate()
    {
        p.worker_start(i, name, 2 * mib);
        p.worker_set_cur(i, (i as u64 + 1) * mib / 3);
    }
    p
}
fn render_sample(width: u16, height: u16) -> Buffer {
    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    term.draw(|f| {
        draw_download(
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
pub fn preview() -> String {
    text(&render_sample(80, 30))
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
                        Color::Black => "#000000".into(),
                        _ => "#000000".into(),
                    };
                    serde_json::json!({"text":c.symbol(),"fg":rgb(c.fg),"bg":rgb(c.bg)})
                })
                .collect();
            std::fs::write(std::path::Path::new(&root).join(name), serde_json::to_vec(&serde_json::json!({"width":buffer.area.width,"height":buffer.area.height,"cells":cells})).unwrap()).unwrap();
        }
    }
    #[test]
    fn slim_download_has_four_workers_and_no_category_panels() {
        let b = render_sample(80, 30);
        let s = text(&b);
        for want in [
            "WORLD OF SURF",
            "Downloaded",
            "50% verified",
            "60/120 files verified",
            "surf_horizon.bsp",
            "coast_materials.pk3",
            "csprogs.dat",
            "04  menu.dat",
            "PREVIEW ONLY",
        ] {
            assert!(s.contains(want), "missing {want}\n{s}");
        }
        for absent in [
            "LIVE TRANSFERS",
            "CONTENT /",
            "MIRROR",
            "RELEASE",
            "╭",
            "╰",
            "~",
        ] {
            assert!(!s.contains(absent));
        }
        assert!(b.content.iter().all(|c| c.bg == BLACK));
        save_snapshot("slim-80x30.json", &b);
    }
    #[test]
    fn full_logo_survives_normal_resizes_and_is_centered() {
        for (w, h) in [(100, 32), (80, 24), (60, 24), (40, 20), (34, 18)] {
            let b = render_sample(w, h);
            let s = text(&b);
            assert!(s.contains(LOGO[0]), "premature compact at{w}x{h}\n{s}");
            assert!(s.contains("WORLD OF SURF"));
            let logo_y = 2 + u16::from(h >= 30);
            let ink: Vec<_> = (0..w).filter(|x| b[(*x, logo_y)].symbol() != " ").collect();
            assert!(ink.first().unwrap().abs_diff(w - ink.last().unwrap() - 1) <= 1);
            save_snapshot(&format!("slim-{w}x{h}.json"), &b);
        }
        assert!(!full_logo(Rect::new(0, 0, 33, 24)));
        assert!(!full_logo(Rect::new(0, 0, 80, 17)));
    }
    #[test]
    fn all_phases_share_black_palette_and_identical_header_geometry() {
        for (w, h) in [(80, 30), (60, 24), (40, 20), (20, 8), (1, 1), (0, 0)] {
            let reference = render_sample(w, h);
            for (name, choice) in [("check", false), ("choice", true), ("install", false)] {
                let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
                t.draw(|f| {
                    if choice {
                        draw_choice(f, "Download World of Surf?", "58.1 MiB", true, 0)
                    } else {
                        draw_status(f, "Checking…", "", 0)
                    }
                })
                .unwrap();
                let b = t.backend().buffer();
                assert!(b.content.iter().all(|c| c.bg == BLACK));
                let header = (if full_logo(b.area) { 10 } else { 2 }) * w as usize;
                assert_eq!(
                    &reference.content[..header.min(reference.content.len())],
                    &b.content[..header.min(b.content.len())]
                );
                if choice && w >= 40 && h >= 20 {
                    let s = text(b);
                    assert!(s.contains("› Yes") && s.contains("  No"));
                    assert!(!s.contains('╭'));
                    save_snapshot(&format!("{name}-{w}x{h}.json"), b);
                }
            }
        }
    }
    #[test]
    fn up_down_and_enter_act_on_selected_answer() {
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut yes = true;
        assert_eq!(choice_key(key(KeyCode::Down), &mut yes), None);
        assert!(!yes);
        assert_eq!(choice_key(key(KeyCode::Enter), &mut yes), Some(false));
        assert_eq!(choice_key(key(KeyCode::Up), &mut yes), None);
        assert!(yes);
        assert_eq!(choice_key(key(KeyCode::Enter), &mut yes), Some(true));
        for code in [KeyCode::Esc, KeyCode::Char('n'), KeyCode::Char('Q')] {
            assert_eq!(choice_key(key(code), &mut yes), Some(false));
        }
        assert_eq!(
            decision(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(false)
        );
        for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
            assert_eq!(
                choice_key(
                    KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, kind),
                    &mut yes
                ),
                None
            );
        }
        assert!(confirm("must not block", true).unwrap());
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
    fn retransmits_never_manufacture_verified_completion() {
        let p = sample();
        p.done_bytes.store(p.total_bytes * 4, Ordering::Relaxed);
        let mut t = Terminal::new(TestBackend::new(80, 30)).unwrap();
        t.draw(|f| draw_download(f, &p, &Rate::default(), 1, false))
            .unwrap();
        let s = text(t.backend().buffer());
        assert!(s.contains("50% verified"));
        assert!(!s.contains("100%"));
        assert!(s.contains("Downloaded 240.0 MiB"));
    }
    #[test]
    fn downloaded_full_worker_still_says_checking_not_verified() {
        let p = Progress::new(100, 1, 1, vec![], Header::default());
        p.worker_start(0, "pending.dat", 100);
        p.worker_set_cur(0, 100);
        let mut t = Terminal::new(TestBackend::new(80, 30)).unwrap();
        t.draw(|f| draw_download(f, &p, &Rate::default(), 0, false))
            .unwrap();
        let s = text(t.backend().buffer());
        assert!(s.contains("Downloaded 0.0 B"));
        assert!(s.contains("checking"));
        assert!(s.contains("0% verified"));
        assert!(s.contains("0/1 files verified"));
    }
    #[test]
    fn zero_half_and_complete_are_distinct_cells() {
        let p = sample();
        let mut frames = Vec::new();
        for n in [0, 60, 120] {
            p.files_done.store(n, Ordering::Relaxed);
            p.verified_bytes
                .store(p.total_bytes * n as u64 / 120, Ordering::Relaxed);
            let mut t = Terminal::new(TestBackend::new(80, 30)).unwrap();
            t.draw(|f| draw_download(f, &p, &Rate::default(), 0, false))
                .unwrap();
            let b = t.backend().buffer().clone();
            assert!(text(&b).contains(&format!("{n}/120 files verified")));
            frames.push(b);
        }
        assert_ne!(frames[0], frames[1]);
        assert_ne!(frames[1], frames[2]);
        assert!(text(&frames[2]).contains("100% verified"));
    }
    #[test]
    fn animated_logo_does_not_invent_download_work() {
        let p = sample();
        let mut previous = None;
        let mut previous_progress = None;
        for tick in 0..46 {
            let mut t = Terminal::new(TestBackend::new(80, 30)).unwrap();
            t.draw(|f| draw_download(f, &p, &Rate::default(), tick, true))
                .unwrap();
            let b = t.backend().buffer();
            let art = &b.content[..880];
            if let Some(old) = &previous {
                assert_ne!(art, old);
            }
            previous = Some(art.to_vec());
            let progress = &b.content[880..];
            if let Some(old) = &previous_progress {
                assert_eq!(progress, old);
            }
            previous_progress = Some(progress.to_vec());
            assert_eq!(p.files_done.load(Ordering::Relaxed), 60);
            assert_eq!(p.verified_bytes.load(Ordering::Relaxed), 30 * 1048576);
        }
    }
    #[test]
    fn incomplete_verification_never_rounds_up_to_100() {
        let p = Progress::new(1000, 300, 4, vec![], Header::default());
        p.files_done.store(299, Ordering::Relaxed);
        p.verified_bytes.store(999, Ordering::Relaxed);
        let mut t = Terminal::new(TestBackend::new(80, 30)).unwrap();
        t.draw(|f| draw_download(f, &p, &Rate::default(), 0, false))
            .unwrap();
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
