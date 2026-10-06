//! Output conventions shared by every command: colour, quiet mode, JSON, tables, and the
//! silent-exit error used for exit codes.

use std::fmt;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lindesk_plugin_api::Level;
use serde::Serialize;

use crate::cli::{ColorWhen, Global};

/// An error that carries only an exit status: the command already printed what matters.
#[derive(Debug)]
pub struct Exit(pub u8);

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "exit status {}", self.0)
    }
}

impl std::error::Error for Exit {}

/// A usage mistake that clap could not catch (exit status 2).
#[derive(Debug)]
pub struct Usage(pub String);

impl fmt::Display for Usage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Usage {}

/// How to talk to the user.
#[derive(Clone, Debug)]
pub struct Ui {
    pub json: bool,
    pub quiet: bool,
    pub system: bool,
    pub color: bool,
    pub err_color: bool,
    pub stdout_tty: bool,
    pub stdin_tty: bool,
}

fn color_enabled(when: ColorWhen, tty: bool) -> bool {
    match when {
        ColorWhen::Always => true,
        ColorWhen::Never => false,
        ColorWhen::Auto => {
            tty && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
                && std::env::var("TERM").map_or(true, |t| t != "dumb")
        }
    }
}

impl Ui {
    pub fn new(g: &Global) -> Ui {
        let stdout_tty = std::io::stdout().is_terminal();
        let stderr_tty = std::io::stderr().is_terminal();
        let unicode = utf8_locale() || g.json;
        ASCII.store(!unicode, Ordering::Relaxed);
        // Never wider than the terminal (JSON is never cut).
        FIT.store(if g.json { 0 } else { term_width().unwrap_or(0) }, Ordering::Relaxed);
        Ui {
            json: g.json,
            quiet: g.quiet,
            system: g.system,
            color: color_enabled(g.color, stdout_tty) && !g.json,
            err_color: color_enabled(g.color, stderr_tty),
            stdout_tty,
            stdin_tty: std::io::stdin().is_terminal(),
        }
    }

    /// A plain UI for tests.
    #[cfg(test)]
    pub fn plain() -> Ui {
        Ui {
            json: false,
            quiet: false,
            system: false,
            color: false,
            err_color: false,
            stdout_tty: false,
            stdin_tty: false,
        }
    }

    pub fn paint(&self, code: &str, s: impl fmt::Display) -> String {
        paint(self.color, code, s)
    }

    pub fn bold(&self, s: impl fmt::Display) -> String {
        self.paint("1", s)
    }

    pub fn dim(&self, s: impl fmt::Display) -> String {
        self.paint("2", s)
    }

    pub fn green(&self, s: impl fmt::Display) -> String {
        self.paint("32", s)
    }

    pub fn yellow(&self, s: impl fmt::Display) -> String {
        self.paint("33", s)
    }

    pub fn red(&self, s: impl fmt::Display) -> String {
        self.paint("31", s)
    }

    pub fn cyan(&self, s: impl fmt::Display) -> String {
        self.paint("36", s)
    }

    pub fn ok_mark(&self) -> String {
        self.green("✓")
    }

    pub fn fail_mark(&self) -> String {
        self.red("✗")
    }

    /// Progress/info for humans, on stderr, suppressed by -q and --json.
    pub fn info(&self, msg: impl fmt::Display) {
        if !self.quiet && !self.json {
            eprintln!("{}", polish(&msg.to_string()));
        }
    }

    /// A warning on stderr (not suppressed by --json, only by -q).
    pub fn warn(&self, msg: impl fmt::Display) {
        if !self.quiet {
            eprintln!("{}", polish(&format!("{} {msg}", paint(self.err_color, "1;33", "warning:"))));
        }
    }

    /// Print a value as pretty JSON on stdout.
    pub fn print_json<T: Serialize + ?Sized>(&self, v: &T) -> anyhow::Result<()> {
        let s = serde_json::to_string_pretty(v)?;
        out_raw(&s);
        Ok(())
    }
}

pub fn paint(on: bool, code: &str, s: impl fmt::Display) -> String {
    if on { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
}

static ASCII: AtomicBool = AtomicBool::new(false);
static FIT: AtomicUsize = AtomicUsize::new(0);

/// Is the locale UTF-8? (`LC_ALL`, then `LC_CTYPE`, then `LANG`; unset means POSIX.)
pub fn utf8_locale() -> bool {
    let v = ["LC_ALL", "LC_CTYPE", "LANG"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()));
    v.is_some_and(|v| {
        let l = v.to_ascii_lowercase();
        l.contains("utf-8") || l.contains("utf8")
    })
}

/// Output only ASCII (non-UTF-8 locale).
pub fn ascii() -> bool {
    ASCII.load(Ordering::Relaxed)
}

/// Terminal width of stdout, when it is a terminal.
pub fn term_width() -> Option<usize> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    if let Some(c) = std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()).filter(|c: &usize| *c > 0) {
        return Some(c);
    }
    crossterm::terminal::size().ok().map(|(w, _)| w as usize).filter(|w| *w > 0)
}

/// The ASCII stand-in for a glyph (same width where possible).
pub fn ascii_char(c: char) -> Option<&'static str> {
    Some(match c {
        '✓' => "+",
        '✗' => "x",
        '●' => "*",
        '○' => "o",
        '◐' | '◌' => "~",
        '⚠' => "!",
        '⚡' => "!",
        '→' => ">",
        '←' => "<",
        '↳' => ">",
        '▶' => ">",
        '—' | '–' => "-",
        '·' => "|",
        '…' => ".",
        '×' => "x",
        'µ' => "u",
        '↺' => "@",
        '⚙' | '✎' => "*",
        '◎' => "o",
        '█' | '▉' | '▊' | '▋' => "#",
        '▌' | '▍' | '▎' | '▏' => "+",
        '░' => ".",
        '▁' => "_",
        '▂' | '▃' => ".",
        '▄' | '▅' => "-",
        '▆' | '▇' => "=",
        '⏸' => "#",
        '🔋' => "BAT",
        '🔌' => "AC",
        '─' | '━' => "-",
        '│' | '┃' => "|",
        '╭' | '╮' | '╰' | '╯' | '┌' | '┐' | '└' | '┘' | '├' | '┤' | '┬' | '┴' | '┼' => "+",
        _ => return None,
    })
}

/// Replace non-ASCII glyphs with ASCII stand-ins.
pub fn asciify(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match ascii_char(c) {
            Some(r) => o.push_str(r),
            None if c.is_ascii() => o.push(c),
            None => o.push('?'),
        }
    }
    o
}

/// Cut a line to `w` visible columns (ANSI-aware), ending in `…`.
pub fn fit(line: &str, w: usize) -> String {
    if width(line) <= w || w < 2 {
        return line.to_string();
    }
    let mut o = String::new();
    let mut seen = 0;
    let mut esc = false;
    let mut colored = false;
    for c in line.chars() {
        if esc {
            o.push(c);
            if c == 'm' {
                esc = false;
            }
            continue;
        }
        if c == '\x1b' {
            esc = true;
            colored = true;
            o.push(c);
            continue;
        }
        let cw = char_width(c);
        if seen + cw > w - 1 {
            break;
        }
        seen += cw;
        o.push(c);
    }
    o.push('…');
    if colored {
        o.push_str("\x1b[0m");
    }
    o
}

/// Apply the output conventions (ASCII fallback, terminal width) to a block of text.
pub fn polish(s: &str) -> String {
    let s = if ascii() { asciify(s) } else { s.to_string() };
    match FIT.load(Ordering::Relaxed) {
        0 => s,
        w => s.lines().map(|l| fit(l, w)).collect::<Vec<_>>().join("\n") + if s.ends_with('\n') { "\n" } else { "" },
    }
}

/// Print data verbatim (query results, JSON, schemas): never cut or transliterated.
pub fn out_raw(s: &str) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(s.as_bytes());
    if !s.ends_with('\n') {
        let _ = o.write_all(b"\n");
    }
    let _ = o.flush();
}

/// Print to stdout, ignoring EPIPE (e.g. `| head`).
pub fn out(s: &str) {
    let s = polish(s);
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(s.as_bytes());
    if !s.ends_with('\n') {
        let _ = o.write_all(b"\n");
    }
    let _ = o.flush();
}

fn char_width(c: char) -> usize {
    match c {
        '🔋' | '🔌' => 2,
        _ => 1,
    }
}

/// Visible width of a string, ignoring ANSI escapes.
pub fn width(s: &str) -> usize {
    let mut w = 0;
    let mut esc = false;
    for c in s.chars() {
        if esc {
            if c == 'm' {
                esc = false;
            }
        } else if c == '\x1b' {
            esc = true;
        } else {
            w += char_width(c);
        }
    }
    w
}

/// Left-align `s` to `w` visible columns.
pub fn pad(s: &str, w: usize) -> String {
    let n = width(s);
    if n >= w { s.to_string() } else { format!("{s}{}", " ".repeat(w - n)) }
}

/// A simple aligned table. Cells may contain ANSI colour.
#[derive(Default)]
pub struct Table {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    right: Vec<usize>,
}

impl Table {
    pub fn new(header: &[&str]) -> Table {
        Table { header: header.iter().map(|s| s.to_string()).collect(), rows: Vec::new(), right: Vec::new() }
    }

    /// Right-align these columns (numbers).
    pub fn right(mut self, cols: &[usize]) -> Table {
        self.right = cols.to_vec();
        self
    }

    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn render(&self, ui: &Ui) -> String {
        let n = self.header.len();
        let mut w = vec![0; n];
        for (i, h) in self.header.iter().enumerate() {
            w[i] = width(h);
        }
        for r in &self.rows {
            for (i, c) in r.iter().enumerate().take(n) {
                w[i] = w[i].max(width(c));
            }
        }
        let line = |cells: &[String], head: bool| {
            let mut s = String::new();
            for (i, c) in cells.iter().enumerate().take(n) {
                let last = i + 1 == n;
                let cell = if self.right.contains(&i) {
                    format!("{}{c}", " ".repeat(w[i].saturating_sub(width(c))))
                } else if last {
                    c.clone()
                } else {
                    pad(c, w[i])
                };
                s.push_str(&if head { ui.bold(&cell) } else { cell });
                if !last {
                    s.push_str("  ");
                }
            }
            s.trim_end().to_string()
        };
        let mut s = line(&self.header, true);
        s.push('\n');
        for r in &self.rows {
            s.push_str(&line(r, false));
            s.push('\n');
        }
        s
    }
}

/// The level as a coloured badge: `● YELLOW`.
pub fn badge(ui: &Ui, l: Level) -> String {
    let code = level_code(l);
    ui.paint(code, format!("● {}", l.as_str().to_uppercase()))
}

/// ANSI SGR code for a level.
pub fn level_code(l: Level) -> &'static str {
    match l {
        Level::Green => "1;32",
        Level::Yellow => "1;33",
        Level::Red => "1;31",
    }
}

/// A smooth unicode bar for a percentage (0..=100), `width` cells, using eighth blocks.
pub fn bar_cells(pct: f64, width: usize) -> String {
    const PARTS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let eighths = ((pct.clamp(0.0, 100.0) / 100.0) * (width * 8) as f64).round() as usize;
    let full = eighths / 8;
    let mut s = "█".repeat(full);
    if full < width {
        let rem = eighths % 8;
        if rem > 0 {
            s.push(PARTS[rem]);
        } else {
            s.push('░');
        }
        s.push_str(&"░".repeat(width - full - 1));
    }
    s
}

/// Colour for a pressure percentage: green below 10, yellow below 30, red above.
pub fn pressure_code(pct: f64) -> &'static str {
    if pct >= 30.0 {
        "31"
    } else if pct >= 10.0 {
        "33"
    } else {
        "32"
    }
}

/// A coloured pressure bar.
pub fn bar(ui: &Ui, pct: f64, width: usize) -> String {
    ui.paint(pressure_code(pct), bar_cells(pct, width))
}

/// `64M/s`.
pub fn rate(bytes_per_s: u64) -> String {
    format!("{}/s", short_bytes(bytes_per_s))
}

/// `512M`, `1.5G`, `64K`.
pub fn short_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let f = n as f64;
    if f >= K * K * K {
        trim(format!("{:.1}", f / (K * K * K))) + "G"
    } else if f >= K * K {
        trim(format!("{:.1}", f / (K * K))) + "M"
    } else if f >= K {
        trim(format!("{:.1}", f / K)) + "K"
    } else {
        n.to_string()
    }
}

fn trim(s: String) -> String {
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

/// A setting as a short human label: `cpu.weight 20`, `io.max 64M/s`, `cpu.idle`.
pub fn setting_short(s: &lindesk_plugin_api::Setting) -> String {
    use lindesk_plugin_api::{IoPrioClass, Setting};
    match s {
        Setting::CpuWeight { value } => format!("cpu.weight {value}"),
        Setting::CpuIdle { value } => {
            if *value {
                "cpu.idle".into()
            } else {
                "cpu.idle off".into()
            }
        }
        Setting::UclampMin { percent } => format!("uclamp.min {}%", trim(format!("{percent:.1}"))),
        Setting::UclampMax { percent } => format!("uclamp.max {}%", trim(format!("{percent:.1}"))),
        Setting::IoWeight { value } => format!("io.weight {value}"),
        Setting::IoMax { limit, .. } => {
            let mut parts = Vec::new();
            match (limit.rbps, limit.wbps) {
                (Some(r), Some(w)) if r == w => parts.push(rate(r)),
                (r, w) => {
                    if let Some(r) = r {
                        parts.push(format!("r {}", rate(r)));
                    }
                    if let Some(w) = w {
                        parts.push(format!("w {}", rate(w)));
                    }
                }
            }
            if let Some(i) = limit.riops.or(limit.wiops) {
                parts.push(format!("{i} iops"));
            }
            if parts.is_empty() { "io.max unlimited".into() } else { format!("io.max {}", parts.join(" ")) }
        }
        Setting::MemoryLow { bytes } => format!("memory.low {}", short_bytes(*bytes)),
        Setting::MemoryHigh { bytes } => match bytes {
            Some(b) => format!("memory.high {}", short_bytes(*b)),
            None => "memory.high max".into(),
        },
        Setting::Nice { value } => format!("nice {value}"),
        Setting::IoPrio { class, level } => match class {
            IoPrioClass::Idle => "ioprio idle".into(),
            IoPrioClass::BestEffort => format!("ioprio be/{level}"),
        },
        Setting::TimerSlack { ns } => format!("timer slack {}ms", ns / 1_000_000),
    }
}

/// Settings joined with ` · `.
pub fn settings_short(s: &[lindesk_plugin_api::Setting]) -> String {
    s.iter().map(setting_short).collect::<Vec<_>>().join(" · ")
}

/// Hardware in one short line: `32×CPU · hybrid 6P+10E · 62 GiB · NVMe · AC`.
pub fn hardware_compact(h: &lindesk_plugin_api::Hardware) -> String {
    use lindesk_plugin_api::{PowerSource, StorageKind};
    let mut parts = Vec::new();
    if h.cpus > 0 {
        parts.push(format!("{}×CPU", h.cpus));
    }
    if h.is_hybrid() {
        parts.push(format!("hybrid {}P+{}E", h.performance_cpus.len(), h.efficiency_cpus.len()));
    }
    if h.mem_total > 0 {
        let gib = h.mem_total as f64 / (1u64 << 30) as f64;
        parts.push(format!("{} GiB", if gib >= 10.0 { format!("{gib:.0}") } else { trim(format!("{gib:.1}")) }));
    }
    let mut kinds: Vec<&str> = Vec::new();
    for d in &h.storage {
        let k = match d.kind {
            StorageKind::Nvme => "NVMe",
            StorageKind::Ssd => "SSD",
            StorageKind::Hdd => "HDD",
            StorageKind::Emmc => "eMMC",
            StorageKind::Sd => "SD",
            StorageKind::Virtual => "virtual disk",
            StorageKind::Network => "network fs",
            StorageKind::Unknown => continue,
        };
        if !kinds.contains(&k) {
            kinds.push(k);
        }
    }
    if !kinds.is_empty() {
        parts.push(kinds.join("+"));
    }
    match h.power {
        PowerSource::Ac => parts.push("AC".into()),
        PowerSource::Battery => parts.push("🔋 battery".into()),
        PowerSource::Unknown => {}
    }
    if let Some(v) = &h.virtualization {
        parts.push(format!("VM:{v}"));
    }
    parts.join(" · ")
}

/// `1.2 GiB`.
pub fn bytes(n: u64) -> String {
    bytesize::ByteSize::b(n).display().iec().to_string()
}

/// `p99` in µs as a short human string.
pub fn micros(us: u64) -> String {
    if us >= 1_000_000 {
        format!("{:.1}s", us as f64 / 1e6)
    } else if us >= 1_000 {
        format!("{:.1}ms", us as f64 / 1e3)
    } else {
        format!("{us}µs")
    }
}

/// `1h02m`, `3m10s`, `12s`, `450ms`.
pub fn duration_ms(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let s = ms / 1000;
    let (h, m, s) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Local wall-clock time of a timestamp.
pub fn clock(ts: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_millis_opt(ts).single() {
        Some(t) => t.format("%H:%M:%S%.3f").to_string(),
        None => ts.to_string(),
    }
}

/// Local date and time of a timestamp.
pub fn datetime(ts: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_millis_opt(ts).single() {
        Some(t) => t.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => ts.to_string(),
    }
}

/// Ask a yes/no question on the terminal. Default no.
pub fn confirm(prompt: &str) -> bool {
    eprint!("{prompt} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Levenshtein distance, for "did you mean" suggestions.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The closest candidate within a sensible distance.
pub fn closest<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let max = (word.chars().count() / 3).max(1) + 1;
    candidates
        .into_iter()
        .map(|c| (edit_distance(word, c), c))
        .filter(|(d, c)| *d <= max || (c.starts_with(word) && word.len() >= 2))
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_and_width() {
        assert_eq!(asciify("✓ ● YELLOW — a · b → c ███▌░"), "+ * YELLOW - a | b > c ###+.");
        assert_eq!(fit("abcdefghij", 5), "abcd…");
        assert_eq!(fit("abc", 5), "abc");
        let colored = paint(true, "32", "abcdefghij");
        assert_eq!(width(&fit(&colored, 5)), 5);
        assert!(fit(&colored, 5).ends_with("\x1b[0m"));
    }

    #[test]
    fn suggestions() {
        let cmds = ["status", "doctor", "simulate", "plugin"];
        assert_eq!(closest("stauts", cmds), Some("status"));
        assert_eq!(closest("doctr", cmds), Some("doctor"));
        assert_eq!(closest("sim", cmds), Some("simulate"));
        assert_eq!(closest("zzzzzz", cmds), None);
    }

    #[test]
    fn table_alignment_ignores_ansi() {
        let ui = Ui::plain();
        let mut t = Table::new(&["NAME", "N"]).right(&[1]);
        t.row(vec![paint(true, "32", "abc"), "5".into()]);
        t.row(vec!["longer-name".into(), "12".into()]);
        let s = t.render(&ui);
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[0], "NAME          N");
        assert_eq!(width(lines[1]), width(lines[2]));
    }

    #[test]
    fn bars_and_settings() {
        assert_eq!(bar_cells(0.0, 4), "░░░░");
        assert_eq!(bar_cells(100.0, 4), "████");
        assert_eq!(bar_cells(50.0, 4), "██░░");
        assert_eq!(bar_cells(56.25, 4), "██▎░");
        assert_eq!(bar_cells(50.0, 4).chars().count(), 4);
        use lindesk_plugin_api::{IoLimit, Setting};
        let io = Setting::IoMax {
            device: "8:0".into(),
            limit: IoLimit { rbps: Some(64 << 20), wbps: Some(64 << 20), ..Default::default() },
        };
        assert_eq!(setting_short(&io), "io.max 64M/s");
        assert_eq!(setting_short(&Setting::CpuWeight { value: 20 }), "cpu.weight 20");
        assert_eq!(setting_short(&Setting::UclampMax { percent: 40.0 }), "uclamp.max 40%");
        assert_eq!(setting_short(&Setting::MemoryLow { bytes: 512 << 20 }), "memory.low 512M");
        assert_eq!(badge(&Ui::plain(), Level::Yellow), "● YELLOW");
    }

    #[test]
    fn hardware_line() {
        use lindesk_plugin_api::{Hardware, PowerSource, StorageDevice, StorageKind};
        let h = Hardware {
            cpus: 32,
            mem_total: 62 * (1 << 30),
            storage: vec![StorageDevice { kind: StorageKind::Nvme, ..Default::default() }],
            power: PowerSource::Ac,
            ..Default::default()
        };
        assert_eq!(hardware_compact(&h), "32×CPU · 62 GiB · NVMe · AC");
        let h = Hardware {
            cpus: 16,
            performance_cpus: (0..12).collect(),
            efficiency_cpus: (12..16).collect(),
            mem_total: 15 * (1 << 29),
            power: PowerSource::Battery,
            ..Default::default()
        };
        assert_eq!(hardware_compact(&h), "16×CPU · hybrid 12P+4E · 7.5 GiB · 🔋 battery");
        assert_eq!(hardware_compact(&Hardware::default()), "");
    }

    #[test]
    fn human_units() {
        assert_eq!(duration_ms(450), "450ms");
        assert_eq!(duration_ms(75_000), "1m15s");
        assert_eq!(duration_ms(3_720_000), "1h02m");
        assert_eq!(micros(950), "950µs");
        assert_eq!(micros(1500), "1.5ms");
    }
}
