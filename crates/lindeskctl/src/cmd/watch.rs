//! `lindeskctl watch`: a full-screen live view (ratatui), a compact line-per-tick mode for
//! pipes and `--plain`, and JSON lines with `--json`.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use lindesk_host::{Explanation, PluginHealth, Status, TargetSummary};
use lindesk_plugin_api::{Level, Resource, Setting, Target};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use serde_json::{Value, json};

use crate::bus::Daemon;
use crate::live::{self, Client, EventMode, FeedItem, PsiHistory};
use crate::ui::{self, Ui, bar_cells, duration_ms, micros, out, setting_short};

const FEED_CAP: usize = 300;
const HISTORY_SECS: f64 = 60.0;

/// Which pane has the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Yielding,
    Protected,
    Events,
}

impl Pane {
    fn next(self) -> Pane {
        match self {
            Pane::Yielding => Pane::Protected,
            Pane::Protected => Pane::Events,
            Pane::Events => Pane::Yielding,
        }
    }
}

/// Everything the screen shows. Pure data, so frames can be rendered in tests.
pub struct App {
    pub color: bool,
    pub source: Option<Daemon>,
    pub status: Option<Status>,
    pub unknown_format: bool,
    pub not_running: Option<String>,
    pub mode: Option<EventMode>,
    pub broker_present: bool,
    pub uptime: Option<Duration>,
    pub psi: PsiHistory,
    pub fg_stall: Vec<(Resource, f64)>,
    pub feed: VecDeque<FeedItem>,
    pub calls_per_s: BTreeMap<String, f64>,
    prev_calls: BTreeMap<String, (u64, Instant)>,
    pub last_interaction: Option<i64>,
    pub paused: bool,
    pub pane: Pane,
    pub selected: [usize; 3],
    pub explain: Option<(String, String)>,
    pub help: bool,
    pub now_ms: i64,
    /// From the daemon's status envelope.
    pub daemon_mode: Option<String>,
    pub epoch: Option<bool>,
    pub hardware: Option<lindesk_plugin_api::Hardware>,
    pub requirements: Vec<String>,
    /// First-run guidance (one line, shown once per login session).
    pub hint: Option<String>,
}

impl App {
    pub fn new(color: bool, interval: Duration) -> App {
        let cap = ((HISTORY_SECS / interval.as_secs_f64().max(0.05)).round() as usize).clamp(10, 600);
        App {
            color,
            source: None,
            status: None,
            unknown_format: false,
            not_running: None,
            mode: None,
            broker_present: false,
            uptime: None,
            psi: PsiHistory::new(cap),
            fg_stall: Vec::new(),
            feed: VecDeque::new(),
            calls_per_s: BTreeMap::new(),
            prev_calls: BTreeMap::new(),
            last_interaction: None,
            paused: false,
            pane: Pane::Yielding,
            selected: [0; 3],
            explain: None,
            help: false,
            now_ms: lindesk_core::clock::now_ms(),
            daemon_mode: None,
            epoch: None,
            hardware: None,
            requirements: Vec::new(),
            hint: None,
        }
    }

    /// Add events (oldest first); the feed keeps the newest at the front.
    pub fn push_events(&mut self, events: &[Value]) {
        for v in events {
            let item = live::describe_any(v);
            if item.kind == "interaction" {
                self.last_interaction = Some(item.ts.max(self.last_interaction.unwrap_or(0)));
                continue; // too chatty for the feed; shown as the ⚡ epoch indicator
            }
            self.feed.push_front(item);
        }
        self.feed.truncate(FEED_CAP);
    }

    /// Update plugin call rates from a new status.
    fn update_rates(&mut self, plugins: &[PluginHealth]) {
        let now = Instant::now();
        for p in plugins {
            if let Some((calls, at)) = self.prev_calls.get(&p.name) {
                let dt = now.duration_since(*at).as_secs_f64();
                if dt > 0.05 {
                    let r = p.stats.calls.saturating_sub(*calls) as f64 / dt;
                    let old = self.calls_per_s.get(&p.name).copied().unwrap_or(r);
                    self.calls_per_s.insert(p.name.clone(), old * 0.5 + r * 0.5);
                }
            }
            self.prev_calls.insert(p.name.clone(), (p.stats.calls, now));
        }
    }

    /// Apply a polled snapshot.
    pub fn apply(&mut self, snap: live::Snapshot, client: Option<&Client>) {
        self.now_ms = lindesk_core::clock::now_ms();
        self.source = snap.source;
        if let Some(c) = client {
            self.mode = c.source().map(|_| c.mode);
            self.uptime = c.uptime();
            self.broker_present = c.broker_present;
            self.not_running =
                if snap.source.is_none() { Some(c.last_error.clone().unwrap_or_default()) } else { None };
        }
        self.unknown_format = snap.raw.is_some() && snap.status.is_none();
        let env = &snap.envelope;
        self.daemon_mode = env.mode.clone();
        if env.uptime.is_some() {
            self.uptime = env.uptime;
        }
        self.epoch = env.epoch;
        if !env.fg_stall.is_empty() {
            self.fg_stall = env.fg_stall.clone();
        }
        if env.hardware.is_some() {
            self.hardware = env.hardware.clone();
        }
        self.requirements = env.requirements.clone();
        if let Some(st) = &snap.status {
            self.update_rates(&st.plugins);
        }
        self.status = snap.status;
        self.push_events(&snap.events);
    }

    fn epoch_active(&self) -> bool {
        if let Some(e) = self.epoch {
            return e;
        }
        let fg_boost = self
            .status
            .as_ref()
            .and_then(|s| s.foreground.as_ref())
            .is_some_and(|f| f.settings.iter().any(|s| matches!(s, Setting::UclampMin { .. })));
        let recent_input = self.last_interaction.is_some_and(|t| self.now_ms - t < 1_000);
        fg_boost || recent_input
    }

    fn rows(&self, pane: Pane) -> usize {
        match (pane, &self.status) {
            (Pane::Yielding, Some(s)) => s.yielding.len(),
            (Pane::Protected, Some(s)) => protected(s).len(),
            (Pane::Events, _) => self.feed.len(),
            _ => 0,
        }
    }

    fn idx(p: Pane) -> usize {
        match p {
            Pane::Yielding => 0,
            Pane::Protected => 1,
            Pane::Events => 2,
        }
    }

    fn move_sel(&mut self, delta: i64) {
        let n = self.rows(self.pane);
        let i = &mut self.selected[Self::idx(self.pane)];
        if n == 0 {
            *i = 0;
            return;
        }
        *i = (*i as i64 + delta).clamp(0, n as i64 - 1) as usize;
    }

    /// The target under the cursor (for explain).
    pub fn selected_target(&self) -> Option<Target> {
        let st = self.status.as_ref();
        let i = self.selected[Self::idx(self.pane)];
        match self.pane {
            Pane::Yielding => st?.yielding.get(i).map(|y| y.summary.target.clone()),
            Pane::Protected => protected(st?).get(i).map(|t| t.target.clone()),
            Pane::Events => {
                let raw = &self.feed.get(i)?.raw;
                serde_json::from_value(raw.get("target")?.clone()).ok()
            }
        }
    }
}

fn protected(st: &Status) -> Vec<&TargetSummary> {
    let mut v: Vec<&TargetSummary> = st.foreground.iter().collect();
    v.extend(st.protected.iter().filter(|p| Some(&p.target) != st.foreground.as_ref().map(|f| &f.target)));
    v
}

// ------------------------------------------------------------------------------ drawing

struct Theme {
    on: bool,
}

impl Theme {
    fn fg(&self, c: Color) -> Style {
        if self.on { Style::default().fg(c) } else { Style::default() }
    }
    fn bold(&self) -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }
    fn dim(&self) -> Style {
        if self.on { Style::default().fg(Color::DarkGray) } else { Style::default() }
    }
    fn level(&self, l: Level) -> Style {
        if !self.on {
            return self.bold();
        }
        let bg = match l {
            Level::Green => Color::Green,
            Level::Yellow => Color::Yellow,
            Level::Red => Color::Red,
        };
        Style::default().bg(bg).fg(Color::Black).add_modifier(Modifier::BOLD)
    }
    fn pressure(&self, pct: f64) -> Style {
        self.fg(if pct >= 30.0 {
            Color::Red
        } else if pct >= 10.0 {
            Color::Yellow
        } else {
            Color::Green
        })
    }
    fn block(&self, title: &str, focused: bool) -> Block<'static> {
        let b = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).title(Span::styled(
            format!(" {title} "),
            if focused { self.bold().patch(self.fg(Color::Cyan)) } else { self.bold() },
        ));
        if focused { b.border_style(self.fg(Color::Cyan)) } else { b.border_style(self.dim()) }
    }
}

/// `▁▂▃▅▇` over the last `width` values, scaled to max(10, peak).
pub fn sparkline(values: &VecDeque<f64>, width: usize) -> String {
    const TICKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let n = values.len().min(width);
    let slice: Vec<f64> = values.iter().skip(values.len() - n).copied().collect();
    let peak = slice.iter().copied().fold(10.0f64, f64::max);
    let mut s: String = " ".repeat(width - n);
    for v in slice {
        let i = ((v / peak) * 7.0).round().clamp(0.0, 7.0) as usize;
        s.push(if v <= 0.0 { ' ' } else { TICKS[i] });
    }
    s
}

fn header(f: &mut Frame, area: Rect, app: &App, th: &Theme) {
    let mut spans: Vec<Span> = Vec::new();
    match (&app.status, app.source) {
        (Some(st), Some(src)) => {
            spans.push(Span::styled(format!(" ● {} ", st.level.as_str().to_uppercase()), th.level(st.level)));
            spans.push(Span::raw("  "));
            spans.push(Span::styled(live::summary(st), th.bold()));
            let mut meta = vec![src.name().to_string()];
            meta.push(match (&app.daemon_mode, src) {
                (Some(m), _) => m.clone(),
                (None, Daemon::Broker) => "broker".into(),
                (None, Daemon::Session) if app.broker_present => "with broker".into(),
                (None, Daemon::Session) => "standalone".into(),
            });
            if st.dry_run {
                meta.push("DRY RUN".into());
            }
            if let Some(u) = app.uptime {
                meta.push(format!("up {}", duration_ms(u.as_millis() as i64)));
            }
            spans.push(Span::styled(format!("   {}", meta.join(" · ")), th.dim()));
            if let Some(h) = app.hardware.as_ref().map(ui::hardware_compact).filter(|h| !h.is_empty()) {
                spans.push(Span::styled(format!(" · {h}"), th.dim()));
            }
        }
        (None, Some(src)) => {
            spans.push(Span::styled(" ? UNKNOWN ", th.bold()));
            spans.push(Span::styled(
                format!("  {} status in an unknown format (version mismatch?)", src.name()),
                th.fg(Color::Yellow),
            ));
        }
        _ => {
            spans.push(Span::styled(
                " ○ OFF ",
                if th.on { Style::default().bg(Color::DarkGray).fg(Color::White) } else { th.bold() },
            ));
            spans.push(Span::styled(
                "  lindesk not running — showing raw pressure",
                th.fg(Color::Yellow).patch(th.bold()),
            ));
            if let Some(h) = app.hardware.as_ref().map(ui::hardware_compact).filter(|h| !h.is_empty()) {
                spans.push(Span::styled(format!("   {h}"), th.dim()));
            }
        }
    }
    let mut lines = vec![Line::from(spans)];
    // Second line: foreground + epoch, or how to start lindesk.
    let mut l2: Vec<Span> = vec![Span::raw(" ")];
    match app.status.as_ref().and_then(|s| s.foreground.as_ref()) {
        Some(fg) => {
            l2.push(Span::styled("foreground ", th.dim()));
            l2.push(Span::styled(fg.label(), th.bold()));
            if app.epoch_active() {
                l2.push(Span::styled("  ⚡ interaction epoch", th.fg(Color::Magenta).patch(th.bold())));
            }
            if !app.fg_stall.is_empty() {
                l2.push(Span::styled("   own stall ", th.dim()));
                for (r, v) in &app.fg_stall {
                    l2.push(Span::styled(format!("{} {:.1}%  ", r.as_str(), v), th.pressure(*v)));
                }
            }
        }
        None if app.source.is_some() => {
            l2.push(Span::styled("no foreground yet (waiting for a focus event)", th.dim()))
        }
        None => {
            let d = app.not_running.clone().unwrap_or_default();
            l2.push(Span::styled(format!("start it: {}", Daemon::Session.start_hint()), th.fg(Color::Cyan)));
            if !d.is_empty() {
                l2.push(Span::styled(format!("   ({d})"), th.dim()));
            }
        }
    }
    if app.paused {
        l2.push(Span::styled("   ⏸ PAUSED (p resumes)", th.fg(Color::Yellow).patch(th.bold())));
    }
    lines.push(Line::from(l2));
    let clock = Paragraph::new(Line::from(Span::styled(
        ui::clock(app.now_ms).get(..8).unwrap_or_default().to_string(),
        th.dim(),
    )))
    .alignment(Alignment::Right);
    f.render_widget(Paragraph::new(lines), area);
    f.render_widget(clock, Rect { height: 1, ..area });
}

fn psi_panel(f: &mut Frame, area: Rect, app: &App, th: &Theme) {
    let inner_w = area.width.saturating_sub(2) as usize;
    // name 8 + bar 20 + " 100.0%" 7 + "  some " 7 + spark + "  full " 7 + spark + " 99.9%" 6
    const FIXED: usize = 8 + 20 + 7 + 7 + 7 + 6;
    let (bar_w, spark_w) = if inner_w >= FIXED + 20 {
        (20usize, ((inner_w - FIXED) / 2).min(240))
    } else {
        (inner_w.saturating_sub(8 + 8).clamp(4, 20), 0)
    };
    let mut lines = Vec::new();
    for (i, r) in Resource::ALL.iter().enumerate() {
        let mut spans = vec![Span::styled(format!("{:<8}", r.as_str()), th.bold())];
        match app.psi.latest[i] {
            Some(p) => {
                let some = p.some.avg10 as f64;
                spans.push(Span::styled(bar_cells(some, bar_w), th.pressure(some)));
                spans.push(Span::styled(format!(" {some:>5.1}%"), th.pressure(some)));
                if spark_w > 0 {
                    spans.push(Span::styled("  some ", th.dim()));
                    spans.push(Span::styled(sparkline(&app.psi.some[i], spark_w), th.pressure(some)));
                    let full = p.full.map_or(0.0, |f| f.avg10 as f64);
                    spans.push(Span::styled("  full ", th.dim()));
                    spans.push(Span::styled(sparkline(&app.psi.full[i], spark_w), th.pressure(full)));
                    spans.push(Span::styled(format!(" {full:>4.1}%"), th.dim()));
                }
            }
            None => spans.push(Span::styled("PSI unavailable", th.dim())),
        }
        lines.push(Line::from(spans));
    }
    let title = format!("Pressure · system avg10 · last {}s", HISTORY_SECS as u32);
    f.render_widget(Paragraph::new(lines).block(th.block(&title, false)), area);
}

fn delta_span(v: Option<f64>, what: &str, th: &Theme) -> Span<'static> {
    match v {
        Some(d) => Span::styled(format!("{d:+.0}% {what}"), if d < -0.5 { th.fg(Color::Yellow) } else { th.dim() }),
        None => Span::styled(format!("· {what}"), th.dim()),
    }
}

fn throttles(settings: &[Setting]) -> String {
    settings.iter().map(setting_short).collect::<Vec<_>>().join(", ")
}

/// Greedily wrap comma-separated items into at most `max` lines of `width` columns.
pub fn wrap_items(items: &[String], width: usize, max: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for it in items {
        match lines.last_mut() {
            Some(l) if l.chars().count() + 2 + it.chars().count() <= width => {
                l.push_str(", ");
                l.push_str(it);
            }
            _ => {
                if let Some(l) = lines.last_mut() {
                    l.push(',');
                }
                lines.push(it.clone());
            }
        }
    }
    if lines.len() > max {
        lines.truncate(max);
        if let Some(l) = lines.last_mut() {
            l.push_str(" …");
        }
    }
    lines
}

fn unit_name(t: &Target) -> String {
    match t {
        Target::Unit { name, .. } => name.clone(),
        t => t.to_string(),
    }
}

fn yielding_table(f: &mut Frame, area: Rect, app: &App, th: &Theme) {
    let focused = app.pane == Pane::Yielding;
    let st = app.status.as_ref();
    const W0: u16 = 28;
    const W1: u16 = 11;
    const W2: u16 = 9;
    let tw = area.width.saturating_sub(2 + W0 + W1 + W2 + 3).max(8) as usize;
    let rows: Vec<Row> = st
        .map(|s| {
            s.yielding
                .iter()
                .map(|y| {
                    let items: Vec<String> = y.summary.settings.iter().map(setting_short).collect();
                    let thr: Vec<Line> = wrap_items(&items, tw, 2)
                        .into_iter()
                        .map(|l| Line::from(Span::styled(l, th.fg(Color::Yellow))))
                        .collect();
                    Row::new(vec![
                        Cell::from(Text::from(vec![
                            Line::from(Span::styled(y.summary.comm.clone().unwrap_or_else(|| "-".into()), th.bold())),
                            Line::from(Span::styled(unit_name(&y.summary.target), th.dim())),
                        ])),
                        Cell::from(y.summary.class.map(|c| c.as_str().to_string()).unwrap_or_default()),
                        Cell::from(Text::from(vec![
                            Line::from(delta_span(y.cpu_delta_pct, "CPU", th)),
                            Line::from(delta_span(y.io_delta_pct, "IO", th)),
                        ])),
                        Cell::from(Text::from(thr)),
                    ])
                    .height(2)
                })
                .collect()
        })
        .unwrap_or_default();
    let n = rows.len();
    let title = format!("Yielding · {n}");
    let block = th.block(&title, focused);
    if n == 0 {
        let msg = if st.is_some() { "nothing is being throttled" } else { "—" };
        f.render_widget(Paragraph::new(Span::styled(msg, th.dim())).block(block), area);
        return;
    }
    let t =
        Table::new(rows, [Constraint::Length(W0), Constraint::Length(W1), Constraint::Length(W2), Constraint::Min(8)])
            .header(Row::new(["PROCESS / UNIT", "CLASS", "VS BEFORE", "THROTTLES"]).style(th.dim()))
            .row_highlight_style(if focused {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            })
            .block(block);
    let mut state = TableState::default().with_selected(Some(app.selected[0].min(n - 1)));
    f.render_stateful_widget(t, area, &mut state);
}

fn protected_table(f: &mut Frame, area: Rect, app: &App, th: &Theme) {
    let focused = app.pane == Pane::Protected;
    let Some(st) = &app.status else {
        f.render_widget(Paragraph::new(Span::styled("—", th.dim())).block(th.block("Protected", focused)), area);
        return;
    };
    let list = protected(st);
    let fg = st.foreground.as_ref().map(|f| &f.target);
    let rows: Vec<Row> = list
        .iter()
        .map(|t| {
            let is_fg = Some(&t.target) == fg;
            Row::new(vec![
                Cell::from(Span::styled(
                    format!("{}{}", if is_fg { "▶ " } else { "  " }, t.label()),
                    if is_fg { th.bold().patch(th.fg(Color::Cyan)) } else { Style::default() },
                )),
                Cell::from(if is_fg {
                    "foreground".to_string()
                } else {
                    t.class.map(|c| c.as_str().to_string()).unwrap_or_default()
                }),
                Cell::from(Span::styled(throttles(&t.settings), th.fg(Color::Green))),
            ])
        })
        .collect();
    let n = rows.len();
    let title = format!("Protected · {n}");
    let t = Table::new(rows, [Constraint::Length(22), Constraint::Length(12), Constraint::Min(10)])
        .header(Row::new(["WORKLOAD", "CLASS", "BOOSTS"]).style(th.dim()))
        .row_highlight_style(if focused { Style::default().add_modifier(Modifier::REVERSED) } else { Style::default() })
        .block(th.block(&title, focused));
    let mut state = TableState::default().with_selected(if n > 0 { Some(app.selected[1].min(n - 1)) } else { None });
    f.render_stateful_widget(t, area, &mut state);
}

fn feed_panel(f: &mut Frame, area: Rect, app: &App, th: &Theme) {
    let focused = app.pane == Pane::Events;
    let h = area.height.saturating_sub(2) as usize;
    let sel = app.selected[2];
    let start = sel.saturating_sub(h.saturating_sub(1));
    let lines: Vec<Line> = app
        .feed
        .iter()
        .enumerate()
        .skip(start)
        .take(h)
        .map(|(i, e)| {
            let style = match e.severity {
                2 => th.fg(Color::Red),
                1 => th.fg(Color::Yellow),
                _ => Style::default(),
            };
            let mut l = Line::from(vec![
                Span::styled(ui::clock(e.ts).get(..8).unwrap_or_default().to_string(), th.dim()),
                Span::raw(" "),
                Span::styled(e.text.clone(), style),
            ]);
            if focused && i == sel {
                l = l.style(Style::default().add_modifier(Modifier::REVERSED));
            }
            l
        })
        .collect();
    let src = match app.mode {
        Some(EventMode::Signals) => "live",
        Some(EventMode::StatusDiff) => "inferred from status",
        None => "offline",
    };
    let title = format!("Events · {src}");
    let body = if lines.is_empty() {
        Text::from(Span::styled("no events yet — level changes, actions and focus changes appear here", th.dim()))
    } else {
        Text::from(lines)
    };
    f.render_widget(Paragraph::new(body).block(th.block(&title, focused)), area);
}

fn plugin_strip(f: &mut Frame, area: Rect, app: &App, th: &Theme) {
    let mut spans: Vec<Span> = vec![Span::styled(" plugins ", th.dim())];
    match &app.status {
        Some(st) if !st.plugins.is_empty() => {
            for p in &st.plugins {
                let (dot, style) = match p.state.as_str() {
                    "running" => ("●", th.fg(Color::Green)),
                    "backoff" | "starting" => ("◐", th.fg(Color::Yellow)),
                    _ => ("○", th.fg(Color::Red)),
                };
                spans.push(Span::styled(format!("{dot} "), style));
                spans.push(Span::styled(p.name.clone(), th.bold()));
                let rate = app.calls_per_s.get(&p.name).copied().unwrap_or(0.0);
                let mut detail = format!(" {rate:.0}/s");
                if p.stats.calls > 0 {
                    detail.push_str(&format!(" p99 {}", micros(p.stats.latency_p99_us)));
                }
                spans.push(Span::styled(detail, th.dim()));
                if p.stats.errors > 0 {
                    spans.push(Span::styled(format!(" {}err", p.stats.errors), th.fg(Color::Yellow)));
                }
                if p.state != "running" {
                    spans.push(Span::styled(format!(" {}", p.state), style));
                }
                spans.push(Span::raw("   "));
            }
        }
        _ => spans.push(Span::styled("—", th.dim())),
    }
    f.render_widget(Paragraph::new(Line::from(spans)).wrap(Wrap { trim: true }), area);
}

fn keys_line(f: &mut Frame, area: Rect, th: &Theme) {
    let k = |s: &'static str| Span::styled(s, th.bold().patch(th.fg(Color::Cyan)));
    let d = |s: &'static str| Span::styled(s, th.dim());
    let line = Line::from(vec![
        k(" q"),
        d(" quit  "),
        k("p"),
        d(" pause  "),
        k("tab"),
        d(" pane  "),
        k("↑↓"),
        d(" select  "),
        k("enter"),
        d(" explain  "),
        k("?"),
        d(" help"),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn explain_panel(f: &mut Frame, area: Rect, title: &str, text: &str, th: &Theme) {
    f.render_widget(Clear, area);
    let block = th.block(&format!("Explain · {title} · esc closes"), true);
    f.render_widget(Paragraph::new(text.to_string()).wrap(Wrap { trim: false }).block(block), area);
}

fn help_popup(f: &mut Frame, area: Rect, th: &Theme) {
    let w = 62.min(area.width);
    let h = 16.min(area.height);
    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    let text = "\
q, esc, ctrl-c   quit
p                pause / resume updates
tab              switch pane: yielding → protected → events
↑ ↓  (k j)       select a row in the focused pane
enter            explain the selected unit (side panel)
esc              close the side panel
?                this help

⚡ marks an interaction epoch: the foreground is boosted for
~500ms after focus or input. Deltas (-31% IO) compare a
throttled unit with its usage before the first throttle.";
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(text).block(th.block("Help", true)), r);
}

/// Render one frame. Degrades on small terminals: sparklines, then plugins, then the
/// protected pane go first.
pub fn draw(f: &mut Frame, app: &App) {
    let th = Theme { on: app.color };
    let area = f.area();
    if area.width < 30 || area.height < 8 {
        let l = match &app.status {
            Some(st) => format!("● {} {}", st.level.as_str().to_uppercase(), live::summary(st)),
            None => "lindesk not running".into(),
        };
        f.render_widget(Paragraph::new(l).wrap(Wrap { trim: true }), area);
        return;
    }
    let tall = area.height >= 24;
    let footer = tall && (app.hint.is_some() || !app.requirements.is_empty());
    let mut constraints = vec![Constraint::Length(2), Constraint::Length(5), Constraint::Min(4)];
    if tall {
        constraints.push(Constraint::Length(1));
    }
    if footer {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1));
    let rows = Layout::vertical(constraints).split(area);
    header(f, rows[0], app, &th);
    psi_panel(f, rows[1], app, &th);
    let body = rows[2];
    let wide = body.width >= 110;
    let (left, right) = if wide {
        let c = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).split(body);
        (c[0], Some(c[1]))
    } else {
        (body, None)
    };
    // Left: yielding over protected (protected dropped when short).
    let show_protected = left.height >= 12;
    let left_parts = if show_protected {
        // Yielding gets what its rows need (two lines each); protected takes the rest.
        let n_yield = app.status.as_ref().map_or(0, |s| s.yielding.len()) as u16;
        let n_prot = app.status.as_ref().map_or(1, |s| protected(s).len()) as u16;
        let prot_min = (n_prot + 3).clamp(4, left.height / 2);
        let yield_h = (2 * n_yield.max(1) + 3).clamp(5, left.height.saturating_sub(prot_min).max(5));
        Layout::vertical([Constraint::Length(yield_h), Constraint::Min(prot_min)]).split(left)
    } else {
        Layout::vertical([Constraint::Min(3)]).split(left)
    };
    match (right, &app.explain) {
        (Some(r), Some((title, text))) => {
            yielding_table(f, left_parts[0], app, &th);
            if show_protected {
                protected_table(f, left_parts[1], app, &th);
            }
            explain_panel(f, r, title, text, &th);
        }
        (Some(r), None) => {
            yielding_table(f, left_parts[0], app, &th);
            if show_protected {
                protected_table(f, left_parts[1], app, &th);
            }
            feed_panel(f, r, app, &th);
        }
        (None, explain) => {
            // Narrow: tables on top, feed below; explain covers the body.
            let parts = Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).split(body);
            let top = if parts[0].height >= 12 {
                Layout::vertical([Constraint::Min(4), Constraint::Length(5)]).split(parts[0])
            } else {
                Layout::vertical([Constraint::Min(3)]).split(parts[0])
            };
            yielding_table(f, top[0], app, &th);
            if top.len() > 1 {
                protected_table(f, top[1], app, &th);
            }
            feed_panel(f, parts[1], app, &th);
            if let Some((title, text)) = explain {
                explain_panel(f, body, title, text, &th);
            }
        }
    }
    let mut i = 3;
    if tall {
        plugin_strip(f, rows[i], app, &th);
        i += 1;
    }
    if footer {
        let l = match &app.hint {
            Some(h) => {
                Line::from(vec![Span::styled(" → ", th.fg(Color::Cyan)), Span::styled(h.clone(), th.fg(Color::Cyan))])
            }
            None => {
                let more = if app.requirements.len() > 1 {
                    format!("  (+{} more: lindeskctl doctor)", app.requirements.len() - 1)
                } else {
                    String::new()
                };
                Line::from(Span::styled(format!(" ⚠ {}{more}", app.requirements[0]), th.dim()))
            }
        };
        f.render_widget(Paragraph::new(l), rows[i]);
        i += 1;
    }
    keys_line(f, rows[i], &th);
    if app.help {
        help_popup(f, area, &th);
    }
}

/// Non-UTF-8 terminals: transliterate every cell.
pub fn asciify_buffer(buf: &mut ratatui::buffer::Buffer) {
    for cell in buf.content.iter_mut() {
        let sym = cell.symbol();
        if !sym.is_ascii() {
            let r = sym.chars().next().and_then(ui::ascii_char).unwrap_or("?");
            let r = if r.len() > 1 { &r[..1] } else { r };
            cell.set_symbol(r);
        }
    }
}

// ------------------------------------------------------------------------------ running

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

struct FgCache {
    target: Option<Target>,
    dir: Option<PathBuf>,
    local_requirements: Option<Vec<String>>,
}

fn tick(app: &mut App, client: &mut Client, fg: &mut FgCache) {
    let snap = client.poll(200);
    let daemon_stall = !snap.envelope.fg_stall.is_empty();
    app.apply(snap, Some(client));
    if app.hardware.is_none() {
        app.hardware = Some(live::local_hardware());
    }
    if app.source.is_none() {
        // No daemon to report them: check the session agent's requirements here, once.
        let local = fg.local_requirements.get_or_insert_with(|| {
            let probe = lindesk_sys::probe(&lindesk_sys::Roots::host(), &lindesk_sys::Env::from_process());
            crate::cmd::doctor::requirement_reports(&probe)
                .into_iter()
                .filter(|r| r.component == lindesk_sys::requirements::Component::Session)
                .flat_map(|r| r.items)
                .filter(|i| i.status != lindesk_sys::requirements::Status::Pass)
                .map(|i| {
                    format!("{}: {}{}", i.title, i.detail, i.remedy.map(|r| format!(" → {r}")).unwrap_or_default())
                })
                .collect()
        });
        app.requirements = local.clone();
    }
    if daemon_stall {
        app.psi.push(&live::read_psi(Path::new("/proc")));
        return;
    }
    app.psi.push(&live::read_psi(Path::new("/proc")));
    let target = app.status.as_ref().and_then(|s| s.foreground.as_ref()).map(|f| f.target.clone());
    if target != fg.target {
        fg.dir = target.as_ref().and_then(|t| live::find_unit_cgroup(Path::new("/sys/fs/cgroup"), t));
        fg.target = target;
    }
    app.fg_stall = fg.dir.as_deref().map(live::cgroup_stall).unwrap_or_default();
}

fn explain_selected(app: &mut App, client: &Client) {
    let Some(t) = app.selected_target() else { return };
    let title = match &t {
        Target::Unit { name, .. } => name.clone(),
        other => other.to_string(),
    };
    let text = match client.explain(&t.to_string()) {
        Ok(raw) => match serde_json::from_str::<Explanation>(&raw) {
            Ok(e) => e.to_string(),
            Err(_) => raw,
        },
        Err(e) => format!("explain failed: {e}"),
    };
    app.explain = Some((title, text));
}

fn tui(ui: &Ui, interval: Duration) -> Result<()> {
    use crossterm::{cursor, execute, terminal};
    let mut stdout = std::io::stdout();
    terminal::enable_raw_mode()?;
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;
    let restore = || {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(std::io::stdout(), terminal::LeaveAlternateScreen, cursor::Show);
    };
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(std::io::stdout(), terminal::LeaveAlternateScreen, cursor::Show);
        hook(info);
    }));
    let result = (|| -> Result<()> {
        let mut term = ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
        let mut app = App::new(ui.color, interval);
        app.hint = crate::hints::once().map(|h| h.text);
        let mut client = Client::new(ui.system);
        let mut fg = FgCache { target: None, dir: None, local_requirements: None };
        tick(&mut app, &mut client, &mut fg);
        let mut next = Instant::now() + interval;
        loop {
            term.draw(|f| {
                draw(f, &app);
                if ui::ascii() {
                    asciify_buffer(f.buffer_mut());
                }
            })?;
            let wait = next.saturating_duration_since(Instant::now());
            if event::poll(wait)? {
                match event::read()? {
                    TermEvent::Key(k) if k.kind != KeyEventKind::Release => match k.code {
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break,
                        KeyCode::Char('q') => break,
                        KeyCode::Esc if app.help => app.help = false,
                        KeyCode::Esc if app.explain.is_some() => app.explain = None,
                        KeyCode::Esc => break,
                        KeyCode::Char('?') => app.help = !app.help,
                        KeyCode::Char('p') | KeyCode::Char(' ') => app.paused = !app.paused,
                        KeyCode::Tab | KeyCode::BackTab => app.pane = app.pane.next(),
                        KeyCode::Up | KeyCode::Char('k') => app.move_sel(-1),
                        KeyCode::Down | KeyCode::Char('j') => app.move_sel(1),
                        KeyCode::PageUp => app.move_sel(-10),
                        KeyCode::PageDown => app.move_sel(10),
                        KeyCode::Enter => explain_selected(&mut app, &client),
                        _ => {}
                    },
                    TermEvent::Resize(..) => {}
                    _ => {}
                }
            }
            if Instant::now() >= next {
                next = Instant::now() + interval;
                if !app.paused {
                    tick(&mut app, &mut client, &mut fg);
                } else {
                    app.now_ms = lindesk_core::clock::now_ms();
                }
            }
        }
        Ok(())
    })();
    restore();
    let _ = std::panic::take_hook();
    result
}

fn psi_compact(app: &App) -> String {
    Resource::ALL
        .iter()
        .enumerate()
        .map(|(i, r)| match app.psi.latest[i] {
            Some(p) => format!("{} {:.1}%", r.as_str(), p.some.avg10),
            None => format!("{} -", r.as_str()),
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// One compact line per tick (pipes, --plain).
pub fn plain_line(ui: &Ui, app: &App) -> String {
    let t = ui::clock(app.now_ms);
    let t = ui.dim(t.get(..8).unwrap_or_default());
    let psi = psi_compact(app);
    match (&app.status, app.source) {
        (Some(st), Some(src)) => {
            let mut s = format!("{t}  {}  ", ui::badge(ui, st.level));
            if let Some(f) = &st.foreground {
                s.push_str(&format!("fg {}{}  ", ui.bold(f.label()), if app.epoch_active() { " ⚡" } else { "" }));
            }
            s.push_str(&format!("{psi}  "));
            if st.yielding.is_empty() {
                s.push_str(&ui.dim("nothing yielding"));
            } else {
                let y: Vec<String> = st.yielding.iter().map(|y| y.to_string()).collect();
                s.push_str(&format!("yielding {}", ui.yellow(y.join(", "))));
            }
            s.push_str(&ui.dim(format!("  [{}]", src.name())));
            s
        }
        _ => format!("{t}  {}  {psi}  {}", ui.dim("○ lindesk not running"), ui.dim("(raw pressure)")),
    }
}

fn plain(ui: &Ui, interval: Duration, count: Option<u64>) -> Result<()> {
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let mut app = App::new(ui.color, interval);
    let mut client = Client::new(ui.system);
    let mut fg = FgCache { target: None, dir: None, local_requirements: None };
    let mut n = 0u64;
    let mut first = true;
    while !STOP.load(Ordering::SeqCst) {
        let started = Instant::now();
        let before = app.feed.len();
        let snap = client.poll(if first { 0 } else { 200 });
        let new_events = snap.events.clone();
        let raw = snap.raw.clone();
        app.apply(snap, Some(&client));
        app.psi.push(&live::read_psi(Path::new("/proc")));
        let target = app.status.as_ref().and_then(|s| s.foreground.as_ref()).map(|f| f.target.clone());
        if target != fg.target {
            fg.dir = target.as_ref().and_then(|t| live::find_unit_cgroup(Path::new("/sys/fs/cgroup"), t));
            fg.target = target;
        }
        if ui.json {
            let psi: BTreeMap<&str, Value> = Resource::ALL
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    (
                        r.as_str(),
                        app.psi.latest[i].map_or(
                            Value::Null,
                            |p| json!({"some_avg10": p.some.avg10, "full_avg10": p.full.map(|f| f.avg10)}),
                        ),
                    )
                })
                .collect();
            ui::out_raw(&serde_json::to_string(&json!({
                "ts": app.now_ms,
                "source": app.source.map(|d| d.name()),
                "level": app.status.as_ref().map(|s| s.level),
                "psi": psi,
                "status": raw,
                "events": new_events,
            }))?);
        } else {
            let mut s = plain_line(ui, &app);
            let added = app.feed.len().saturating_sub(before).min(app.feed.len());
            for e in app.feed.iter().take(added).collect::<Vec<_>>().into_iter().rev() {
                let style = match e.severity {
                    2 => ui.red(&e.text),
                    1 => ui.yellow(&e.text),
                    _ => e.text.clone(),
                };
                s.push_str(&format!("\n{}  ↳ {style}", ui.dim(ui::clock(e.ts).get(..8).unwrap_or_default())));
            }
            out(&s);
        }
        let _ = std::io::stdout().flush();
        first = false;
        n += 1;
        if count.is_some_and(|c| n >= c) {
            break;
        }
        while started.elapsed() < interval && !STOP.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(25).min(interval));
        }
    }
    Ok(())
}

pub fn watch(ui: &Ui, interval: Option<Duration>, count: Option<u64>, force_plain: bool) -> Result<()> {
    let interactive = ui.stdout_tty && ui.stdin_tty && !ui.json && !force_plain && count.is_none();
    if interactive {
        tui(ui, interval.unwrap_or(Duration::from_millis(250)))
    } else {
        plain(ui, interval.unwrap_or(Duration::from_secs(1)), count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// A realistic status as lindesk-session serialises it.
    pub const FIXTURE: &str = r#"{
      "now": 1791100000000, "host": "lindesk-session", "level": "yellow", "dry_run": false,
      "foreground": {"target": {"type": "unit", "manager": {"kind": "user", "uid": 1000}, "name": "app-gnome-org.mozilla.firefox-4242.scope"},
                     "comm": "firefox", "class": "interactive", "foreground": true,
                     "settings": [{"knob": "cpu_weight", "value": 500}, {"knob": "uclamp_min", "percent": 30.0}]},
      "protected": [
        {"target": {"type": "unit", "manager": {"kind": "system"}, "name": "session.slice"}, "class": "critical",
         "settings": [{"knob": "cpu_weight", "value": 200}, {"knob": "memory_low", "bytes": 536870912}]},
        {"target": {"type": "unit", "manager": {"kind": "user", "uid": 1000}, "name": "pipewire.service"}, "comm": "pipewire", "class": "critical", "settings": []}
      ],
      "yielding": [
        {"target": {"type": "unit", "manager": {"kind": "user", "uid": 1000}, "name": "lindesk-run-cargo-77.scope"}, "comm": "rustc", "class": "batch",
         "settings": [{"knob": "cpu_idle", "value": true}, {"knob": "io_weight", "value": 1},
                      {"knob": "io_max", "device": "259:0", "limit": {"rbps": 67108864, "wbps": 67108864}}],
         "since": 1791099870000, "cpu_delta_pct": -12.0, "io_delta_pct": -31.0},
        {"target": {"type": "unit", "manager": {"kind": "user", "uid": 1000}, "name": "tracker-miner-fs-3.service"}, "comm": "tracker-miner-f", "class": "background",
         "settings": [{"knob": "cpu_weight", "value": 20}, {"knob": "uclamp_max", "percent": 60.0}], "since": 1791099990000, "io_delta_pct": -8.0}
      ],
      "plugins": [
        {"name": "threshold", "kinds": ["policy"], "origin": {"type": "builtin"}, "state": "running",
         "stats": {"calls": 14211, "errors": 0, "timeouts": 0, "failures": 0, "restarts": 0, "latency_ewma_us": 210.0, "latency_p99_us": 310}},
        {"name": "steam-games", "kinds": ["classifier"], "origin": {"type": "external", "manifest": "/home/u/.local/share/lindesk/plugins/steam-games/plugin.toml"},
         "state": "backoff", "detail": "retrying in 2000ms",
         "stats": {"calls": 12, "errors": 3, "timeouts": 1, "failures": 1, "restarts": 1, "latency_ewma_us": 900.0, "latency_p99_us": 900}}
      ]
    }"#;

    fn psi(some: f32, full: f32) -> Option<lindesk_sys::psi::PsiReading> {
        let l = |v| lindesk_sys::psi::PsiLine { avg10: v, avg60: v, avg300: v, total_us: 0 };
        Some(lindesk_sys::psi::PsiReading { some: l(some), full: Some(l(full)) })
    }

    pub fn fixture_app() -> App {
        let st: Status = serde_json::from_str(FIXTURE).expect("fixture parses as lindesk_host::Status");
        let mut app = App::new(false, Duration::from_millis(250));
        app.source = Some(Daemon::Session);
        app.broker_present = true;
        app.mode = Some(EventMode::Signals);
        app.uptime = Some(Duration::from_secs(2 * 3600 + 13 * 60));
        app.now_ms = st.now;
        for i in 0..240 {
            let wave = ((i as f32) / 12.0).sin().abs();
            app.psi.push(&[
                (Resource::Cpu, psi(8.0 * wave, 0.0)),
                (Resource::Io, psi(31.2 * wave + 5.0, 12.0 * wave)),
                (Resource::Memory, psi(0.0, 0.0)),
            ]);
        }
        app.fg_stall = vec![(Resource::Cpu, 0.4), (Resource::Io, 3.1), (Resource::Memory, 0.0)];
        app.daemon_mode = Some("connected".into());
        app.hardware = Some(lindesk_plugin_api::Hardware {
            cpus: 32,
            mem_total: 62 << 30,
            storage: vec![lindesk_plugin_api::StorageDevice {
                kind: lindesk_plugin_api::StorageKind::Nvme,
                ..Default::default()
            }],
            power: lindesk_plugin_api::PowerSource::Ac,
            ..Default::default()
        });
        app.requirements = vec!["io.weight: inert → enable iocost (io.iocost = \"auto\")".into()];
        app.push_events(&[
            json!({"kind": "focus", "ts": st.now - 40_000, "app_id": "org.mozilla.firefox", "source": "gnome"}),
            json!({"kind": "level", "ts": st.now - 30_000, "from": "green", "to": "yellow", "policy": "threshold"}),
            json!({"kind": "action_applied", "ts": st.now - 29_750,
                   "target": {"type": "unit", "manager": {"kind": "user", "uid": 1000}, "name": "lindesk-run-cargo-77.scope"},
                   "setting": {"knob": "io_weight", "value": 1},
                   "provenance": {"plugin": "threshold", "reason": "YELLOW: io pressure 31% > 20%; batch yields", "priority": 0}}),
            json!({"kind": "plugin_error", "ts": st.now - 5_000, "plugin": "steam-games", "message": "classifier.classify timed out"}),
        ]);
        app.status = Some(st);
        app
    }

    fn screen(app: &App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        let mut s = String::new();
        for y in 0..h {
            for x in 0..w {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn renders_a_full_frame_from_status_json() {
        let app = fixture_app();
        let s = screen(&app, 140, 36);
        println!("{s}");
        for needle in [
            "● YELLOW",
            "protecting firefox; 2 workloads yielding",
            "lindesk-session · connected · up 2h13m · 32×CPU · 62 GiB · NVMe · AC",
            "⚠ io.weight: inert",
            "foreground firefox",
            "⚡",
            "interaction epoch",
            "own stall cpu 0.4%  io 3.1%",
            "Pressure · system avg10",
            "Yielding · 2",
            "rustc",
            "-31% IO",
            "lindesk-run-cargo-77.scope",
            "cpu.idle, io.weight 1",
            "io.max 64M/s",
            "cpu.weight 20,",
            "uclamp.max 60%",
            "Protected · 3",
            "▶ firefox",
            "session.slice",
            "memory.low 512M",
            "Events · live",
            "level GREEN → YELLOW",
            "apply  io.weight 1 on lindesk-run-cargo-77.scope",
            "plugin steam-games: classifier.classify timed out",
            "● threshold",
            "◐ steam-games",
            "q quit",
        ] {
            assert!(s.contains(needle), "missing {needle:?} in frame:\n{s}");
        }
        // Newest event first.
        assert!(s.find("plugin steam-games:").unwrap() < s.find("level GREEN → YELLOW").unwrap());
    }

    #[test]
    fn degrades_on_small_terminals_and_without_daemon() {
        let app = fixture_app();
        let s = screen(&app, 80, 20);
        assert!(s.contains("● YELLOW") && s.contains("Yielding") && s.contains("Events"), "{s}");
        let s = screen(&app, 28, 6);
        assert!(s.contains("YELLOW"), "{s}");
        let mut off = App::new(false, Duration::from_millis(250));
        off.psi.push(&[(Resource::Cpu, psi(3.0, 0.0)), (Resource::Io, psi(40.0, 10.0)), (Resource::Memory, None)]);
        let s = screen(&off, 120, 30);
        assert!(s.contains("lindesk not running — showing raw pressure"), "{s}");
        assert!(s.contains("40.0%") && s.contains("PSI unavailable"), "{s}");
        assert!(s.contains("systemctl --user enable --now lindesk-session"), "{s}");
    }

    #[test]
    fn keys_move_selection_and_explain_target() {
        let mut app = fixture_app();
        assert_eq!(app.selected_target().unwrap().to_string(), "user/1000:lindesk-run-cargo-77.scope");
        app.move_sel(1);
        assert_eq!(app.selected_target().unwrap().to_string(), "user/1000:tracker-miner-fs-3.service");
        app.move_sel(5);
        assert_eq!(app.selected[0], 1, "clamped");
        app.pane = app.pane.next();
        assert_eq!(app.selected_target().unwrap().to_string(), "user/1000:app-gnome-org.mozilla.firefox-4242.scope");
        app.explain = Some(("x.scope".into(), "user/1000:x.scope\n  class:   batch".into()));
        let s = screen(&app, 140, 36);
        assert!(s.contains("Explain · x.scope") && s.contains("class:   batch"), "{s}");
        app.help = true;
        assert!(screen(&app, 140, 36).contains("interaction epoch: the foreground"));
    }

    #[test]
    fn plain_lines() {
        let app = fixture_app();
        let l = plain_line(&Ui::plain(), &app);
        assert!(l.contains("● YELLOW  fg firefox ⚡"), "{l}");
        assert!(l.contains("io 3") && l.contains("yielding rustc -12% CPU -31% IO"), "{l}");
        let off = App::new(false, Duration::from_secs(1));
        assert!(plain_line(&Ui::plain(), &off).contains("○ lindesk not running"));
        assert_eq!(sparkline(&VecDeque::from(vec![0.0, 5.0, 10.0]), 5), "   ▅█");
        let items: Vec<String> = ["cpu.idle", "io.weight 1", "io.max 64M/s"].iter().map(|s| s.to_string()).collect();
        assert_eq!(wrap_items(&items, 22, 2), vec!["cpu.idle, io.weight 1,", "io.max 64M/s"]);
        assert_eq!(wrap_items(&items, 100, 2), vec!["cpu.idle, io.weight 1, io.max 64M/s"]);
        assert_eq!(wrap_items(&items, 5, 2), vec!["cpu.idle,", "io.weight 1, …"]);
    }
}
