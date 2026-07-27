//! Download progress UI, drawn with ratatui: two rounded, coloured boxes
//! (Overall Progress + Worker Threads). Because ratatui lays out on a fixed cell
//! grid with right-aligned numeric columns, digit-count changes never shift the
//! layout. Falls back to silence when stdout isn't a TTY (piped/CI).

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::crossterm::{cursor, execute};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, LineGauge, Paragraph};
use ratatui::{Frame, Terminal};
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const C_OVERALL_BORDER: Color = Color::Blue;
const C_OVERALL_TITLE: Color = Color::LightCyan;
const C_WORKER_BORDER: Color = Color::Green;
const C_WORKER_TITLE: Color = Color::LightGreen;
const C_BAR_FILLED: Color = Color::LightMagenta;
const C_BAR_EMPTY: Color = Color::DarkGray;
const C_LABEL: Color = Color::Green;
const C_NAME: Color = Color::LightCyan;
const C_SIZE: Color = Color::Rgb(180, 210, 120);
const C_SPEED: Color = Color::LightRed;
const C_PCT: Color = Color::LightMagenta;
const C_ETA: Color = Color::LightBlue;
const C_DIM: Color = Color::DarkGray;

pub struct WorkerSlot {
    pub cur: AtomicU64,
    pub total: AtomicU64,
    pub active: AtomicBool,
    pub name: Mutex<String>,
}

impl WorkerSlot {
    fn new() -> Self {
        Self {
            cur: AtomicU64::new(0),
            total: AtomicU64::new(0),
            active: AtomicBool::new(false),
            name: Mutex::new(String::from("idle")),
        }
    }
}

/// One content bucket in the breakdown panel ("Models", "Maps", …).
pub struct CategorySlot {
    pub name: &'static str,
    pub total: u64,
    pub done: AtomicU64,
}

/// Static context shown above the bars. The download UI takes over the alternate
/// screen, so anything printed before it starts is invisible while it runs — this
/// keeps "what am I downloading, onto what, from where" on screen the whole time.
#[derive(Clone, Default)]
pub struct Header {
    pub system: String,
    pub platform: String,
    pub release: String,
    pub mirror: String,
}

/// Which bucket a manifest path belongs to. Paths look like "quakers/models/..."
/// or a bare "fteqw64.exe" at the install root.
pub fn category_of(path: &str) -> &'static str {
    let Some((_gamedir, rest)) = path.split_once('/') else {
        return "Engine";
    };
    match rest.split_once('/') {
        Some((top, tail)) => match top {
            "models" => "Models",
            "maps" => "Maps",
            "sounds" => "Sounds",
            "textures" => "Textures",
            // gfx/ is mostly HUD and menu art, but gfx/decals (bullet holes, sprays) and
            // gfx/env (skyboxes) are world textures by any sane reading — bucket them with
            // the textures so the breakdown reflects what's actually being fetched.
            "gfx" => match tail.split('/').next().unwrap_or("") {
                "decals" | "env" => "Textures",
                _ => "Interface",
            },
            "sprites" => "Interface",
            _ => "Game data",
        },
        // gamedir-root files: the big pk3 packs dominate the payload, so they get
        // their own bucket rather than vanishing into "Game data".
        None => {
            if rest.rsplit('.').next() == Some("pk3") {
                "Packs"
            } else {
                "Game data"
            }
        }
    }
}

/// Display order for the breakdown panel; buckets with nothing to fetch are dropped.
pub const CATEGORY_ORDER: [&str; 8] = [
    "Packs", "Models", "Textures", "Maps", "Sounds", "Interface", "Game data", "Engine",
];

/// Shared, lock-light progress state. Hot counters are atomics; only the current
/// filename (changes once per file) is behind a Mutex.
pub struct Progress {
    pub total_bytes: u64,
    pub done_bytes: AtomicU64,
    pub files_total: usize,
    pub files_done: AtomicUsize,
    pub workers: Vec<WorkerSlot>,
    pub categories: Vec<CategorySlot>,
    pub header: Header,
}

impl Progress {
    pub fn new(
        total_bytes: u64,
        files_total: usize,
        n_workers: usize,
        categories: Vec<(&'static str, u64)>,
        header: Header,
    ) -> Arc<Self> {
        Arc::new(Self {
            total_bytes,
            done_bytes: AtomicU64::new(0),
            files_total,
            files_done: AtomicUsize::new(0),
            workers: (0..n_workers).map(|_| WorkerSlot::new()).collect(),
            categories: categories
                .into_iter()
                .map(|(name, total)| CategorySlot {
                    name,
                    total,
                    done: AtomicU64::new(0),
                })
                .collect(),
            header,
        })
    }

    /// Index into `categories` for a manifest path — resolved once per job at setup,
    /// never on the byte-counting hot path.
    pub fn category_index(&self, path: &str) -> Option<usize> {
        let want = category_of(path);
        self.categories.iter().position(|c| c.name == want)
    }

    pub fn add_category_bytes(&self, idx: Option<usize>, n: u64) {
        if let Some(i) = idx {
            self.categories[i].done.fetch_add(n, Ordering::Relaxed);
        }
    }
    pub fn add_bytes(&self, n: u64) {
        self.done_bytes.fetch_add(n, Ordering::Relaxed);
    }
    pub fn file_done(&self) {
        self.files_done.fetch_add(1, Ordering::Relaxed);
    }
    pub fn worker_start(&self, w: usize, name: &str, total: u64) {
        let s = &self.workers[w];
        *s.name.lock().unwrap() = name.to_string();
        s.total.store(total, Ordering::Relaxed);
        s.cur.store(0, Ordering::Relaxed);
        s.active.store(true, Ordering::Relaxed);
    }
    pub fn worker_set_cur(&self, w: usize, cur: u64) {
        self.workers[w].cur.store(cur, Ordering::Relaxed);
    }
    pub fn worker_add(&self, w: usize, n: u64) {
        self.workers[w].cur.fetch_add(n, Ordering::Relaxed);
    }
    pub fn worker_idle(&self, w: usize) {
        let s = &self.workers[w];
        s.active.store(false, Ordering::Relaxed);
        *s.name.lock().unwrap() = String::from("idle");
    }
}

/// Rolling per-worker + overall throughput, smoothed with an EMA.
struct Speeds {
    last: Instant,
    prev_done: u64,
    prev: Vec<u64>,
    overall_bps: f64,
    wbps: Vec<f64>,
}

impl Speeds {
    fn new(n: usize) -> Self {
        Self {
            last: Instant::now(),
            prev_done: 0,
            prev: vec![0; n],
            overall_bps: 0.0,
            wbps: vec![0.0; n],
        }
    }
    fn sample(&mut self, p: &Progress) {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f64();
        if dt < 0.05 {
            return;
        }
        let done = p.done_bytes.load(Ordering::Relaxed);
        let inst = done.saturating_sub(self.prev_done) as f64 / dt;
        self.overall_bps = ema(self.overall_bps, inst);
        self.prev_done = done;
        for (i, slot) in p.workers.iter().enumerate() {
            let c = slot.cur.load(Ordering::Relaxed);
            let inst = c.saturating_sub(self.prev[i]) as f64 / dt;
            self.wbps[i] = ema(self.wbps[i], inst);
            self.prev[i] = c;
        }
        self.last = now;
    }
}

fn ema(old: f64, new: f64) -> f64 {
    if old == 0.0 {
        new
    } else {
        0.6 * old + 0.4 * new
    }
}

fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if b < 1024 {
        return format!("{} B", b);
    }
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < 4 {
        v /= 1024.0;
        i += 1;
    }
    format!("{:.1} {}", v, U[i])
}

fn eta(done: u64, total: u64, bps: f64) -> String {
    if bps <= 1.0 || done >= total {
        return "  --:--".to_string();
    }
    let secs = ((total - done) as f64 / bps) as u64;
    format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let tail: String = chars[chars.len() - (max - 1)..].iter().collect();
        format!("…{}", tail)
    }
}

pub fn view_height(n_workers: usize, n_categories: usize) -> u16 {
    let header = if n_categories > 0 { 5 } else { 0 };
    let cats = if n_categories > 0 { n_categories as u16 + 2 } else { 0 };
    header + 3 + cats + n_workers as u16 + 2
}

fn draw(f: &mut Frame, p: &Progress, sp: &Speeds) {
    let n = p.workers.len() as u16;
    let ncat = p.categories.len() as u16;
    let has_header = !p.header.release.is_empty();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(if has_header { 5 } else { 0 }),
            Constraint::Length(3),
            Constraint::Length(if ncat > 0 { ncat + 2 } else { 0 }),
            Constraint::Length(n + 2),
            Constraint::Min(0), // absorb the rest of the (full-screen) area
        ])
        .split(f.area());
    if has_header {
        draw_header(f, rows[0], p);
    }
    draw_overall(f, rows[1], p, sp);
    if ncat > 0 {
        draw_categories(f, rows[2], p);
    }
    draw_workers(f, rows[3], p, sp);
}

fn draw_header(f: &mut Frame, area: Rect, p: &Progress) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(C_OVERALL_BORDER))
        .title(
            Line::from(Span::styled(
                " QUAKERS · The Religious Society of Friends · ALPHA ",
                Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            ))
            .centered(),
        );
    let inner = block.inner(area);
    f.render_widget(block, area);

    let row = |label: &str, value: String| {
        Line::from(vec![
            Span::styled(
                format!("{:<9}", label),
                Style::new().fg(C_LABEL).add_modifier(Modifier::BOLD),
            ),
            Span::styled(value, Style::new().fg(C_NAME)),
        ])
    };
    let lines = vec![
        row(" System", p.header.system.clone()),
        row(
            " Release",
            format!("{}   ({} build)", p.header.release, p.header.platform),
        ),
        row(" Mirror", p.header.mirror.clone()),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_categories(f: &mut Frame, area: Rect, p: &Progress) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(C_WORKER_BORDER))
        .title(
            Line::from(Span::styled(
                " Content ",
                Style::new().fg(C_WORKER_TITLE).add_modifier(Modifier::BOLD),
            ))
            .centered(),
        );
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::vertical(vec![Constraint::Length(1); p.categories.len()]).split(inner);
    for (i, c) in p.categories.iter().enumerate() {
        let done = c.done.load(Ordering::Relaxed).min(c.total);
        let ratio = if c.total > 0 {
            (done as f64 / c.total as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let complete = done >= c.total;
        let cols = Layout::horizontal([
            Constraint::Length(12), // "Textures"
            Constraint::Min(6),     // gauge
            Constraint::Length(6),  // " 100%"
            Constraint::Length(23), // "  1.1 GB /   1.8 GB"
        ])
        .split(rows[i]);

        f.render_widget(
            Paragraph::new(Span::styled(
                c.name,
                Style::new()
                    .fg(if complete { C_DIM } else { C_LABEL })
                    .add_modifier(Modifier::BOLD),
            )),
            cols[0],
        );
        f.render_widget(
            LineGauge::default()
                .line_set(symbols::line::THICK)
                .filled_style(Style::new().fg(if complete { C_BAR_EMPTY } else { C_BAR_FILLED }))
                .unfilled_style(Style::new().fg(C_BAR_EMPTY))
                .ratio(ratio)
                .label(Span::raw("")),
            cols[1],
        );
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("{:>3}%", (ratio * 100.0) as u16),
                Style::new().fg(C_PCT).add_modifier(Modifier::BOLD),
            ))
            .right_aligned(),
            cols[2],
        );
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("{:>9} /{:>9}", human(done), human(c.total)),
                Style::new().fg(C_SIZE),
            ))
            .right_aligned(),
            cols[3],
        );
    }
}

fn draw_overall(f: &mut Frame, area: Rect, p: &Progress, sp: &Speeds) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(C_OVERALL_BORDER))
        .title(
            Line::from(Span::styled(
                " Overall Progress ",
                Style::new().fg(C_OVERALL_TITLE).add_modifier(Modifier::BOLD),
            ))
            .centered(),
        );
    let inner = block.inner(area);
    f.render_widget(block, area);

    let done = p.done_bytes.load(Ordering::Relaxed).min(p.total_bytes);
    let ratio = if p.total_bytes > 0 {
        (done as f64 / p.total_bytes as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let cols = Layout::horizontal([
        Constraint::Length(13),
        Constraint::Min(8),
        Constraint::Length(34),
    ])
    .split(inner);

    f.render_widget(
        Paragraph::new(Span::styled(
            "Total Files",
            Style::new().fg(C_LABEL).add_modifier(Modifier::BOLD),
        )),
        cols[0],
    );
    f.render_widget(
        LineGauge::default()
            .line_set(symbols::line::THICK)
            .filled_style(Style::new().fg(C_BAR_FILLED))
            .unfilled_style(Style::new().fg(C_BAR_EMPTY))
            .ratio(ratio)
            .label(Span::raw("")),
        cols[1],
    );

    let fd = p.files_done.load(Ordering::Relaxed);
    let w = p.files_total.to_string().len();
    let stats = Line::from(vec![
        Span::styled(
            format!("{:>3}%", (ratio * 100.0) as u16),
            Style::new().fg(C_PCT).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("({:>w$}/{})", fd, p.files_total, w = w),
            Style::new().fg(Color::White),
        ),
        Span::raw("  "),
        Span::styled(eta(done, p.total_bytes, sp.overall_bps), Style::new().fg(C_ETA)),
    ]);
    f.render_widget(Paragraph::new(stats).right_aligned(), cols[2]);
}

fn draw_workers(f: &mut Frame, area: Rect, p: &Progress, sp: &Speeds) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(C_WORKER_BORDER))
        .title(
            Line::from(Span::styled(
                " Worker Threads ",
                Style::new().fg(C_WORKER_TITLE).add_modifier(Modifier::BOLD),
            ))
            .centered(),
        );
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rowh = vec![Constraint::Length(1); p.workers.len()];
    let rows = Layout::vertical(rowh).split(inner);
    for (i, slot) in p.workers.iter().enumerate() {
        draw_worker_row(f, rows[i], i, slot, sp.wbps[i]);
    }
}

fn draw_worker_row(f: &mut Frame, area: Rect, i: usize, slot: &WorkerSlot, bps: f64) {
    let active = slot.active.load(Ordering::Relaxed);
    let cur = slot.cur.load(Ordering::Relaxed);
    let total = slot.total.load(Ordering::Relaxed);
    let name = slot.name.lock().unwrap().clone();
    let ratio = if total > 0 {
        (cur as f64 / total as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let cols = Layout::horizontal([
        Constraint::Length(38), // "Worker N: <name>"
        Constraint::Min(6),     // gauge
        Constraint::Length(21), // "  cur / total"
        Constraint::Length(11), // "  x.y MB/s"
    ])
    .split(area);

    let label = Line::from(vec![
        Span::styled(
            format!("Worker {}: ", i + 1),
            Style::new().fg(C_LABEL).add_modifier(Modifier::BOLD),
        ),
        Span::styled(truncate(&name, 26), Style::new().fg(if active { C_NAME } else { C_DIM })),
    ]);
    f.render_widget(Paragraph::new(label), cols[0]);

    f.render_widget(
        LineGauge::default()
            .line_set(symbols::line::THICK)
            .filled_style(Style::new().fg(if active { C_BAR_FILLED } else { C_BAR_EMPTY }))
            .unfilled_style(Style::new().fg(C_BAR_EMPTY))
            .ratio(ratio)
            .label(Span::raw("")),
        cols[1],
    );

    if active {
        let sizes = format!("{:>9} /{:>9}", human(cur), human(total));
        f.render_widget(
            Paragraph::new(Span::styled(sizes, Style::new().fg(C_SIZE))).right_aligned(),
            cols[2],
        );
        let spd = format!("{:>7}/s", human(bps as u64));
        f.render_widget(
            Paragraph::new(Span::styled(spd, Style::new().fg(C_SPEED))).right_aligned(),
            cols[3],
        );
    }
}

/// Start the live UI in its own thread (only when stdout is a real terminal).
/// Returns a stop flag + join handle; call `stop.store(true)` then join to finish.
pub fn start(progress: Arc<Progress>) -> Option<(Arc<AtomicBool>, JoinHandle<()>)> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    let n = progress.workers.len();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let handle = thread::spawn(move || {
        // Full-screen (alternate screen). Terminal::draw() calls autoresize() every
        // frame, so window resizes reflow cleanly instead of corrupting the display.
        let _ = execute!(std::io::stdout(), EnterAlternateScreen, cursor::Hide);
        let backend = CrosstermBackend::new(std::io::stdout());
        let mut term = match Terminal::new(backend) {
            Ok(t) => t,
            Err(_) => {
                let _ = execute!(std::io::stdout(), LeaveAlternateScreen, cursor::Show);
                return;
            }
        };
        let mut sp = Speeds::new(n);
        loop {
            sp.sample(&progress);
            let _ = term.draw(|f| draw(f, &progress, &sp));
            if stop2.load(Ordering::Relaxed) {
                break;
            }
            thread::sleep(Duration::from_millis(120));
        }
        let _ = term.draw(|f| draw(f, &progress, &sp));
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen, cursor::Show);
    });
    Some((stop, handle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_real_manifest_shapes() {
        // engine files sit at the install root, with no gamedir prefix
        assert_eq!(category_of("fteqw64.exe"), "Engine");
        assert_eq!(category_of("fteqw-gl64"), "Engine");
        assert_eq!(category_of("default.fmf"), "Engine");
        // the big packs live at the gamedir root and dominate the payload
        assert_eq!(category_of("quakers/polyhaven_props.pk3"), "Packs");
        assert_eq!(category_of("quakers/pizzadoggy.pk3"), "Packs");
        // ordinary content dirs
        assert_eq!(category_of("quakers/models/props/misc/outrun/car_0.skin"), "Models");
        assert_eq!(category_of("quakers/maps/e1m1.bsp"), "Maps");
        assert_eq!(category_of("quakers/sounds/weapons/rocket.ogg"), "Sounds");
        assert_eq!(category_of("quakers/textures/wall.dds"), "Textures");
        assert_eq!(category_of("quakers/gfx/conback.lmp"), "Interface");
        assert_eq!(category_of("quakers/sprites/flame.spr"), "Interface");
        // decals and skyboxes are world textures, not HUD art
        assert_eq!(category_of("quakers/gfx/decals/impacts/hole1.png"), "Textures");
        assert_eq!(category_of("quakers/gfx/env/sky_01_up.png"), "Textures");
        assert_eq!(category_of("quakers/gfx/rain/streak2.png"), "Interface");
        assert_eq!(category_of("quakers/glsl/water.glsl"), "Game data");
        assert_eq!(category_of("quakers/default.cfg"), "Game data");
    }

    #[test]
    fn every_bucket_is_displayable() {
        // anything category_of can return must have a slot in the display order,
        // or its bytes would be silently dropped from the breakdown.
        for p in [
            "fteqw64.exe",
            "quakers/polyhaven.pk3",
            "quakers/models/x.md3",
            "quakers/maps/x.bsp",
            "quakers/sounds/x.ogg",
            "quakers/textures/x.dds",
            "quakers/gfx/x.lmp",
            "quakers/anythingelse/x.dat",
            "quakers/x.cfg",
        ] {
            assert!(
                CATEGORY_ORDER.contains(&category_of(p)),
                "{p} -> {} missing from CATEGORY_ORDER",
                category_of(p)
            );
        }
    }

    #[test]
    fn category_totals_conserve_bytes() {
        // the breakdown must account for every byte of the download set exactly once
        let files: Vec<(&str, u64)> = vec![
            ("fteqw-gl64", 100),
            ("quakers/polyhaven.pk3", 900),
            ("quakers/models/a.md3", 30),
            ("quakers/models/b.md3", 70),
            ("quakers/maps/m.bsp", 500),
            ("quakers/zz.cfg", 1),
        ];
        let total: u64 = files.iter().map(|f| f.1).sum();
        let summed: u64 = CATEGORY_ORDER
            .iter()
            .map(|name| {
                files
                    .iter()
                    .filter(|f| category_of(f.0) == *name)
                    .map(|f| f.1)
                    .sum::<u64>()
            })
            .sum();
        assert_eq!(total, summed);
    }
}

/// Best-effort human name for the machine we're running on, with no extra crates.
/// Linux gets its real distro string from /etc/os-release; Windows gets its build
/// number from `cmd /c ver`; anything else falls back to the compile-time OS name.
pub fn system_description() -> String {
    let arch = std::env::consts::ARCH;

    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/etc/os-release") {
            for line in s.lines() {
                if let Some(v) = line.strip_prefix("PRETTY_NAME=") {
                    return format!("{}  ·  {}", v.trim_matches('"'), arch);
                }
            }
        }
        return format!("Linux  ·  {}", arch);
    }

    #[cfg(target_os = "windows")]
    {
        // "Microsoft Windows [Version 10.0.26100.1234]" -> "Windows 10.0.26100.1234"
        if let Ok(out) = std::process::Command::new("cmd").args(["/c", "ver"]).output() {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(v) = s.split("[Version").nth(1).and_then(|t| t.split(']').next()) {
                return format!("Windows {}  ·  {}", v.trim(), arch);
            }
        }
        return format!("Windows  ·  {}", arch);
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    format!("{}  ·  {}", std::env::consts::OS, arch)
}

/// Render one frame with sample data to a string (no TTY needed) — used by
/// `--ui-preview` to eyeball the layout/alignment.
pub fn preview() -> String {
    use ratatui::backend::TestBackend;
    let mb = 1024u64 * 1024;
    let total = 6_800 * mb;
    let cats: Vec<(&'static str, u64)> = vec![
        ("Packs", 3_200 * mb),
        ("Models", 1_100 * mb),
        ("Textures", 1_200 * mb),
        ("Maps", 705 * mb),
        ("Sounds", 40 * mb),
        ("Interface", 131 * mb),
        ("Game data", 1 * mb),
        ("Engine", 22 * mb),
    ];
    let header = Header {
        system: system_description(),
        platform: crate::manifest::platform_key().to_string(),
        release: "alpha · 2026.07.27_1".to_string(),
        mirror: "dl.proto.bar".to_string(),
    };
    let p = Progress::new(total, 13786, 4, cats, header);
    p.done_bytes.store((total as f64 * 0.19) as u64, Ordering::Relaxed);
    p.files_done.store(2603, Ordering::Relaxed);
    for (i, frac) in [0.31, 0.12, 0.44, 0.05, 1.0, 0.62, 1.0, 1.0].iter().enumerate() {
        let c = &p.categories[i];
        c.done.store((c.total as f64 * frac) as u64, Ordering::Relaxed);
    }
    let sample: [(&str, u64, u64); 4] = [
        ("coast_rocks_01_4k.blend", (18.7 * mb as f64) as u64, (45.9 * mb as f64) as u64),
        ("coast_rocks_01_nor_gl_4k.exr", (3.1 * mb as f64) as u64, (30.7 * mb as f64) as u64),
        ("coast_rocks_01_diff_8k.jpg", (30.6 * mb as f64) as u64, (33.4 * mb as f64) as u64),
        ("coast_rocks_01_nor_gl_8k.exr", (58.2 * mb as f64) as u64, (109.3 * mb as f64) as u64),
    ];
    for (i, (n, c, t)) in sample.iter().enumerate() {
        p.worker_start(i, n, *t);
        p.worker_set_cur(i, *c);
    }
    let mut sp = Speeds::new(4);
    sp.overall_bps = 11.7 * mb as f64;
    sp.wbps = vec![3.9 * mb as f64, 1.4 * mb as f64, 2.7 * mb as f64, 3.7 * mb as f64];

    let w = 96u16;
    let backend = TestBackend::new(w, view_height(4, p.categories.len()));
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| draw(f, &p, &sp)).unwrap();

    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}
