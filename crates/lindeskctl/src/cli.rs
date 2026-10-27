//! The command-line grammar. Everything a user can type is declared here, so `--help`,
//! completions and man pages are generated from one place.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use lindesk_plugin_api::{PluginKind, QosClass};

const ROOT_AFTER_HELP: &str = "\
Examples:
  lindeskctl doctor                         what this machine supports, and what's running
  lindeskctl status                         level, foreground, protected and yielding work
  lindeskctl watch                          live dashboard (q to quit)
  lindeskctl events -f                      stream level changes and actions as they happen
  lindeskctl run --batch make -j32          run a build that yields to the desktop
  lindeskctl explain firefox                why a process has the settings it has
  lindeskctl profile list                   which profile is active, and why
  lindeskctl hardware                       what lindesk detected (CPU types, disks, latency map)
  lindeskctl swap                           swap analysis with ready-to-apply fixes
  lindeskctl changes apply --sysctl vm.swappiness=10 --ttl 10m
                                            a guardrailed, versioned, auto-reverting change
  lindeskctl mcp                            connect a local AI agent (claude mcp add lindesk -- lindesk-mcp)
  lindeskctl plugin new --kind classifier --lang python steam-games
  lindeskctl simulate --policy ./my-policy --from -1h
  lindeskctl query \"select resource, avg(some_avg10) from pressure group by 1\"

Without a command, lindeskctl prints a compact status. Global flags work before or after
the subcommand. `lindeskctl <cmd> --help` has details
and examples for each command. Unknown commands run `lindeskctl-<cmd>` from PATH, like git.";

/// macOS-style QoS for the Linux desktop: inspect, steer and extend lindesk.
#[derive(Parser, Debug)]
#[command(
    name = "lindeskctl",
    version = lindesk_version::LONG,
    about = "Inspect, steer and extend lindesk, the desktop QoS governor",
    long_about = "lindeskctl is the command-line front door to lindesk, a userspace QoS governor \
                  that keeps the app you are using responsive while builds, copies and indexers \
                  run in the background.\n\n\
                  It talks to lindesk-session (session bus) by default and to the lindeskd broker \
                  (system bus) with --system.",
    after_help = ROOT_AFTER_HELP,
    allow_external_subcommands = true,
    propagate_version = true,
    max_term_width = 100
)]
pub struct Cli {
    #[command(flatten)]
    pub global: Global,
    /// Without a command: a compact status.
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}

/// Flags accepted by every command.
#[derive(Args, Debug, Clone, Default)]
pub struct Global {
    /// Machine-readable JSON output.
    #[arg(long, global = true)]
    pub json: bool,
    /// Talk to lindeskd and the system lake instead of the session agent and your lake.
    #[arg(long, global = true)]
    pub system: bool,
    /// When to use colours.
    #[arg(long, global = true, value_enum, value_name = "WHEN", default_value_t = ColorWhen::Auto)]
    pub color: ColorWhen,
    /// Print less: only errors and the essential result.
    #[arg(short, long, global = true)]
    pub quiet: bool,
}

#[derive(ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorWhen {
    #[default]
    Auto,
    Always,
    Never,
}

/// QoS classes as a CLI value.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClassArg {
    Critical,
    Interactive,
    Normal,
    Background,
    Batch,
}

impl From<ClassArg> for QosClass {
    fn from(c: ClassArg) -> QosClass {
        match c {
            ClassArg::Critical => QosClass::Critical,
            ClassArg::Interactive => QosClass::Interactive,
            ClassArg::Normal => QosClass::Normal,
            ClassArg::Background => QosClass::Background,
            ClassArg::Batch => QosClass::Batch,
        }
    }
}

/// Plugin kinds as a CLI value.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum KindArg {
    Sensor,
    Adapter,
    Classifier,
    Policy,
    Actuator,
    Sink,
}

impl From<KindArg> for PluginKind {
    fn from(k: KindArg) -> PluginKind {
        match k {
            KindArg::Sensor => PluginKind::Sensor,
            KindArg::Adapter => PluginKind::Adapter,
            KindArg::Classifier => PluginKind::Classifier,
            KindArg::Policy => PluginKind::Policy,
            KindArg::Actuator => PluginKind::Actuator,
            KindArg::Sink => PluginKind::Sink,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LangArg {
    Python,
    Rust,
    Shell,
}

impl From<LangArg> for lindesk_host::Lang {
    fn from(l: LangArg) -> Self {
        match l {
            LangArg::Python => lindesk_host::Lang::Python,
            LangArg::Rust => lindesk_host::Lang::Rust,
            LangArg::Shell => lindesk_host::Lang::Shell,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellArg {
    Bash,
    Zsh,
    Fish,
    Elvish,
    Powershell,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum QueryFormat {
    /// Aligned table (default on a terminal).
    #[default]
    Table,
    Csv,
    Json,
    /// Export as a ZSTD Parquet file (for other tools); needs --out.
    Parquet,
}

/// How the telemetry lake's Vortex files are read.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BackendArg {
    /// The DuckDB vortex extension when it loads, else the built-in reader.
    #[default]
    Auto,
    /// Always the DuckDB vortex extension (fails if it can't be loaded).
    Extension,
    /// Always the built-in reader (offline; slower, memory-bound).
    Native,
}

/// Parse "1s", "250ms", "2m".
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let d = humantime::parse_duration(s.trim()).map_err(|e| format!("{e} (examples: 500ms, 1s, 2m)"))?;
    if d.is_zero() {
        return Err("must be greater than zero".into());
    }
    Ok(d)
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Check what this machine supports and whether lindesk is running.
    #[command(
        long_about = "Probe the kernel, cgroups, PSI, I/O stack, desktop and conflicting daemons, \
                      then check that lindeskd, lindesk-session and the desktop adapter are \
                      reachable, and summarise the hardware. Every warning comes with a concrete \
                      remedy.\n\n\
                      --preflight checks only the hard requirements of lindeskd, lindesk-session \
                      and lindeskctl (what the installers run): a pass/warn/fail checklist with \
                      remedies; with -q only warnings and failures are printed.\n\n\
                      Exit status: plain doctor 0 when nothing needs action, 1 when a warning or \
                      error does; --preflight 1 only when a requirement fails.",
        after_help = "Examples:\n  lindeskctl doctor\n  lindeskctl doctor --json | jq .services\n  lindeskctl doctor --preflight\n  lindeskctl doctor --preflight --quiet   # installers"
    )]
    Doctor {
        /// Check only the requirements (installers and package scripts run this).
        #[arg(long)]
        preflight: bool,
    },

    /// Show the current level, foreground app, and protected and yielding workloads.
    #[command(
        long_about = "Ask lindesk-session for its status (falling back to lindeskd), and render \
                      the health level, the foreground app, what is protected, what is yielding \
                      and how much, and every plugin's health. --system asks lindeskd only.",
        after_help = "Examples:\n  lindeskctl status\n  lindeskctl status --json | jq .level\n  lindeskctl status --system"
    )]
    Status,

    /// Inspect bounded latency history or submit an application timing observation.
    #[command(subcommand)]
    Latency(LatencyCmd),

    /// Inspect a process's DRM GPU activity and resident memory (driver dependent).
    Gpu {
        pid: i32,
        #[arg(long, default_value = "500ms", value_parser = parse_duration)]
        interval: Duration,
    },

    /// Protect a same-user dependency while its client is interactive (one hop).
    Depend {
        /// Client process that is waiting.
        client: i32,
        /// Service process it is waiting for.
        server: i32,
        /// Lease length, at most 30s. Renew explicitly for longer requests.
        #[arg(long, default_value = "5s", value_parser = parse_duration, conflicts_with = "cancel")]
        ttl: Duration,
        /// Cancel this client/server pair immediately.
        #[arg(long)]
        cancel: bool,
    },

    /// Run a JSON array of command argv arrays with pressure-adaptive admission.
    Batch {
        /// JSON file: [["make", "-C", "project-a", "-j2"], ["make", "-C", "project-b", "-j2"]].
        file: PathBuf,
        /// Maximum concurrent commands; recovery adds one slot after 2s of low pressure.
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u16).range(1..=256))]
        jobs: u16,
    },

    /// Live full-screen view: level, PSI, protected/yielding workloads and events.
    #[command(
        long_about = "A real-time dashboard, refreshed every 250ms: the level and what lindesk is \
                      protecting, system PSI gauges with 60-second sparklines (and the foreground's \
                      own stall), the protected and yielding workloads with their boosts, throttles \
                      and CPU/IO deltas, a live event feed, and plugin health.\n\n\
                      Keys: q quit, p pause, tab switch pane, ↑/↓ select, enter explain the \
                      selected unit, ? help.\n\n\
                      Without a terminal, or with --plain, one compact line is printed per second \
                      (plus new events); --json prints JSON lines. When lindesk isn't running it \
                      still shows the raw system pressure.",
        after_help = "Examples:\n  lindeskctl watch\n  lindeskctl watch --plain\n  lindeskctl watch --json --count 10 > samples.jsonl"
    )]
    Watch {
        /// Refresh interval (default 250ms full-screen, 1s plain).
        #[arg(long, short = 'i', value_parser = parse_duration)]
        interval: Option<Duration>,
        /// Stop after this many refreshes (implies --plain).
        #[arg(long, short = 'n')]
        count: Option<u64>,
        /// Line-per-tick output instead of the full-screen view.
        #[arg(long)]
        plain: bool,
    },

    /// Show recent events, or stream them live with --follow.
    #[command(
        long_about = "Print the daemon's recent events (level changes, actions applied and reverted \
                      with their reasons, focus changes, class changes, plugin errors). --follow \
                      keeps streaming them as they happen. --json prints one event per line, as \
                      the daemon serialises it.",
        after_help = "Examples:\n  lindeskctl events\n  lindeskctl events -f --kind level --kind action\n  lindeskctl events -f --json | jq -c 'select(.kind == \"plugin_error\")'"
    )]
    Events {
        /// Keep streaming new events.
        #[arg(long, short)]
        follow: bool,
        /// Only these kinds (repeatable): focus, interaction, class_set, level, action, action_applied, action_reverted, plugin_error, notice.
        #[arg(long = "kind", short = 'k', value_name = "KIND")]
        kinds: Vec<String>,
        /// How many recent events to show first.
        #[arg(long, short = 'n', default_value_t = 50)]
        lines: usize,
    },

    /// Explain why a process or unit has the class and settings it has.
    #[command(
        long_about = "Ask the daemon which class a target has and who decided it, which settings \
                      lindesk holds on it, which plugin and intent set each one, and any clamps, \
                      rejections or errors.\n\n\
                      WHAT is a pid, a unit (app-firefox-1234.scope, user/1000:app.slice, \
                      system:docker.service), or a process name resolved like pidof. A name that \
                      matches processes in several units is an error listing the candidates.",
        after_help = "Examples:\n  lindeskctl explain firefox\n  lindeskctl explain $(pidof -s rustc)\n  lindeskctl explain user/1000:app-org.gnome.Terminal.slice\n  lindeskctl explain --system docker.service"
    )]
    Explain {
        /// pid, unit/target, or process name.
        what: String,
    },

    /// Run a command in a QoS class (for example a build that yields to the desktop).
    #[command(
        long_about = "Start COMMAND in its own transient systemd scope in the slice that matches \
                      the class (lindesk-batch.slice for --batch, background.slice for \
                      --background, app.slice otherwise), then tell lindesk-session its class. \
                      The exit status of COMMAND is returned and signals are forwarded.\n\n\
                      Without a systemd user manager, --batch and --background fall back to \
                      nice/ioprio (nice 19 + idle I/O, nice 10 + low best-effort I/O).",
        after_help = "Examples:\n  lindeskctl run --batch make -j32\n  lindeskctl run --batch --name kernel -- make -C linux -j\"$(nproc)\"\n  lindeskctl run --background rsync -a ~/photos /mnt/backup\n  lindeskctl run --class critical -- obs"
    )]
    Run(RunArgs),

    /// Set the QoS class of a running process.
    #[command(
        long_about = "Pin the class of a process (and the unit it runs in). Your own processes go \
                      through lindesk-session; --system asks lindeskd, which checks polkit and \
                      can change other users' processes.",
        after_help = "Examples:\n  lindeskctl set $(pidof -s rsync) background\n  lindeskctl set blender batch\n  lindeskctl set --system 4242 batch"
    )]
    Set {
        /// pid or process name.
        what: String,
        /// The class to set.
        #[arg(value_enum)]
        class: ClassArg,
    },

    /// Undo every change lindesk made (session, or system with --system).
    #[command(
        long_about = "Restore every setting lindesk changed to its original value, from the \
                      applied-state file. Daemons do this themselves on exit and after a crash; \
                      this is the manual escape hatch. A running daemon would re-apply its \
                      settings, so pass --stop to stop it first (its unit restores on stop).",
        after_help = "Examples:\n  lindeskctl restore --stop\n  sudo lindeskctl restore --system --stop"
    )]
    Restore {
        /// Stop the running daemon first (systemctl stop), which also restores.
        #[arg(long)]
        stop: bool,
    },

    /// Create, test, install and inspect plugins.
    #[command(subcommand)]
    Plugin(PluginCmd),

    /// Replay recorded telemetry through a policy and show what it would have done.
    #[command(
        long_about = "Read pressure, per-unit usage and focus/interaction events for a time window \
                      from the telemetry lake, rebuild the units in memory, and step the real \
                      pipeline engine through the window on simulated time: once with the \
                      candidate policy (alongside the baseline, as in production) and once with \
                      the baseline alone. Prints the level timeline, a diff of actions, and how \
                      long the candidate would have throttled what. Nothing is applied.\n\n\
                      --policy is a plugin directory or builtin:threshold. Times are RFC 3339, \
                      YYYY-MM-DD[ HH:MM[:SS]], now, or relative like -1h / -30m / 2h (ago).",
        after_help = "Examples:\n  lindeskctl simulate --policy ./my-policy --from -1h\n  lindeskctl simulate --policy builtin:threshold --config ./tuned.toml --from -6h --to -5h\n  lindeskctl simulate --policy ./my-policy --baseline none --json | jq .summary"
    )]
    Simulate(SimulateArgs),

    /// Run read-only SQL over the telemetry lake (DuckDB).
    #[command(
        long_about = "Run one read-only statement (SELECT, WITH, FROM, VALUES, DESCRIBE, SHOW, \
                      SUMMARIZE) against your telemetry lake, or the system lake with --system \
                      (readable by the lindesk group). Tables: pressure, units, events, metrics, \
                      bench_runs, bench_samples and the rollups pressure_1m/_1h, units_1m/_1h.",
        after_help = "Examples:\n  lindeskctl query \"select resource, avg(some_avg10) from pressure group by 1\"\n  lindeskctl query \"from events where kind = 'level' order by ts desc limit 20\"\n  lindeskctl query --format csv \"select * from units_1h\" > units.csv\n  lindeskctl query --format parquet --out pressure.parquet \"from pressure\""
    )]
    Query(QueryArgs),

    /// Turn lindesk on: enable and start the daemons (and handle conflicting daemons).
    #[command(after_help = "Examples:\n  lindeskctl on\n  sudo lindeskctl on --all-users --disable-conflicts")]
    On(OnArgs),

    /// Turn lindesk off: back to the machine's default behaviour, without uninstalling.
    #[command(
        long_about = "Stop and disable lindeskd and lindesk-session (both restore everything they \
                      changed when they stop), put conflicting daemons back to their \
                      install-time state (unless --keep-conflicts) and record that lindesk is off, \
                      so upgrades don't turn it back on. Config, profiles, change history and \
                      telemetry are kept. `lindeskctl on` reverses it.",
        after_help = "Examples:\n  lindeskctl off\n  sudo lindeskctl off --all-users --yes"
    )]
    Off(OffArgs),

    /// Pause: revert everything lindesk applied and only observe, without stopping it.
    #[command(
        long_about = "Every running daemon (your session agent, and lindeskd when allowed) reverts \
                      the settings it applied and keeps sensing and deciding without applying \
                      anything: an instant A/B switch. With --for it resumes by itself.",
        after_help = "Examples:\n  lindeskctl pause --for 10m\n  lindeskctl pause          # until lindeskctl resume"
    )]
    Pause {
        /// Resume automatically after this long (`30m`, `1h`).
        #[arg(long = "for", value_parser = parse_duration, value_name = "DURATION")]
        duration: Option<Duration>,
    },

    /// End a pause early.
    Resume,

    /// Machine setup: conflicting priority daemons (`setup conflicts`).
    #[command(subcommand)]
    Setup(SetupCmd),

    /// Maintain the telemetry lake: catalog, compact, prune, stats, delete, purge, shell, install-extension.
    #[command(subcommand)]
    Data(DataCmd),

    /// Benchmarks (runs lindesk-bench with the remaining arguments).
    #[command(
        disable_help_flag = true,
        long_about = "Pass every argument through to lindesk-bench on PATH. See `lindeskctl bench --help`.",
        after_help = "Examples:\n  lindeskctl bench run --label baseline\n  lindeskctl bench compare"
    )]
    Bench {
        /// Arguments for lindesk-bench.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, value_name = "ARGS")]
        args: Vec<OsString>,
    },

    /// Check or show the configuration.
    #[command(subcommand)]
    Config(ConfigCmd),

    /// List, inspect and pick CPU profiles (auto-selected overlays on the config).
    #[command(subcommand)]
    Profile(ProfileCmd),

    /// Versioned, guardrailed changes: plan, apply, confirm, revert, history.
    #[command(subcommand, visible_alias = "change")]
    Changes(ChangesCmd),

    /// Measure this machine's latency baseline: CPU wakeup, memory, disks, swap.
    #[command(
        long_about = "Measure the latency map lindesk scales its defaults with: per-CPU timer-wakeup \
                      latency and relative speed (P vs E cores), memory load latency and bandwidth, \
                      4 KiB random-read latency and sequential bandwidth of each managed disk, and \
                      swap page-in cost. Takes about 15 s (--quick: about 3 s); keep the machine \
                      idle.\n\n\
                      With lindeskd running, the broker measures and stores the system map \
                      (/var/lib/lindesk/calibration.json; polkit may ask for authentication). \
                      Without it, lindeskctl measures as your user and saves \
                      ~/.local/state/lindesk/calibration.json, which lindesk-session uses.\n\n\
                      A busy machine (PSI above 10%) is refused unless --force, which marks the \
                      map NOISY and skips the disks while I/O pressure is high.",
        after_help = "Examples:\n  lindeskctl calibrate\n  lindeskctl calibrate --quick --force\n  lindeskctl calibrate --json | jq .map.memory"
    )]
    Calibrate {
        /// About 3 s instead of 15 s: fewer samples, smaller buffers.
        #[arg(long)]
        quick: bool,
        /// Run even when the machine is busy (results are marked NOISY).
        #[arg(long)]
        force: bool,
    },

    /// Show the full hardware profile lindesk adapts to, with the latency map.
    #[command(
        long_about = "Show what lindesk detected and adapts to: CPU architecture, vendor, topology \
                      (hybrid P/E or big.LITTLE cores, SMT), cpufreq driver/governor/EPP, EAS and \
                      ITMT, NUMA; memory and swap; each managed disk with its kind and I/O \
                      scheduler; power source and chassis; virtualization; and the calibrated \
                      latency map when there is one. Asks the daemon (GetHardware), else probes \
                      locally.",
        after_help = "Examples:\n  lindeskctl hardware\n  lindeskctl hardware --json | jq .features"
    )]
    Hardware,

    /// Analyze swap (zram, zswap, vm.* tunables, MGLRU) and propose fixes.
    #[command(
        long_about = "Inspect swap devices and what backs them, zswap, the reclaim sysctls \
                      (swappiness, page-cluster, watermark_scale_factor, ...), MGLRU, swap-in/out \
                      rates and the units with the most swapped-out memory. Every finding that \
                      lindesk can fix comes with a ready-to-run `lindeskctl changes apply` \
                      command (guardrailed, versioned, revertible).",
        after_help = "Examples:\n  lindeskctl swap\n  lindeskctl swap --json | jq '.findings[].proposed_ops'"
    )]
    Swap,

    /// The MCP server for local AI agents: status, client config, serve.
    #[command(
        long_about = "lindesk-mcp lets a local AI agent (Claude Code, Claude Desktop, Cursor, ...) \
                      assess this machine and apply guardrailed, versioned, auto-reverting \
                      changes. Without a subcommand: `status`.\n\n\
                      Most clients start it over stdio and need no service: \
                      `claude mcp add lindesk -- lindesk-mcp`. For clients that connect to a \
                      running endpoint, enable the user socket: \
                      `systemctl --user enable --now lindesk-mcp.socket`.",
        after_help = "Examples:\n  lindeskctl mcp\n  lindeskctl mcp config --client claude-desktop\n  lindeskctl mcp serve --listen 127.0.0.1:7457"
    )]
    Mcp {
        #[command(subcommand)]
        cmd: Option<McpCmd>,
    },

    /// Print a shell completion script.
    #[command(
        long_about = "Print a completion script for SHELL to stdout.",
        after_help = "Examples:\n  lindeskctl completions bash > ~/.local/share/bash-completion/completions/lindeskctl\n  lindeskctl completions zsh > ~/.zfunc/_lindeskctl\n  lindeskctl completions fish > ~/.config/fish/completions/lindeskctl.fish"
    )]
    Completions {
        #[arg(value_enum)]
        shell: ShellArg,
    },

    /// Write man pages for lindeskctl and the daemons into a directory.
    #[command(
        long_about = "Write lindeskctl.1, one page per subcommand (lindeskctl-plugin.1, \
                      lindeskctl-plugin-new.1, ...), and lindeskd.8, lindesk-session.1 and \
                      lindesk-mcp.1 into DIR. (lindesk-data.1 comes with lindesk-data, from the \
                      optional lindesk-telemetry package: lindesk-data man DIR.)",
        after_help = "Example:\n  lindeskctl man /tmp/man && man -l /tmp/man/lindeskctl.1"
    )]
    Man {
        /// Output directory (created if missing).
        dir: PathBuf,
    },

    #[command(external_subcommand)]
    External(Vec<OsString>),
}

#[derive(Subcommand, Debug)]
pub enum LatencyCmd {
    /// Show recent metrics with p95/p99; --json also exports pressure and engine context.
    Show,
    /// Record an application measurement; audio-xruns is a delta count, all others ms.
    Report {
        #[arg(value_parser = ["frame", "input", "runqueue", "audio-xruns", "timer"])]
        kind: String,
        value: f64,
    },
    /// Measure timer wakeup lateness (not input-to-display latency) and save observations.
    Probe {
        #[arg(long, default_value = "5s", value_parser = parse_duration)]
        duration: Duration,
    },
}

#[derive(Args, Debug)]
#[command(group = clap::ArgGroup::new("qos").multiple(false))]
pub struct RunArgs {
    /// Run as interactive (app.slice).
    #[arg(long, group = "qos")]
    pub interactive: bool,
    /// Run as batch: yields first under pressure (lindesk-batch.slice).
    #[arg(long, group = "qos")]
    pub batch: bool,
    /// Run as background (background.slice).
    #[arg(long, group = "qos")]
    pub background: bool,
    /// Run with an explicit class.
    #[arg(long, value_enum, group = "qos")]
    pub class: Option<ClassArg>,
    /// Experimental EEVDF slice for the launched command (100..100000 us, fair scheduler only).
    #[arg(long, value_parser = clap::value_parser!(u32).range(100..=100_000))]
    pub slice_us: Option<u32>,
    /// Name for the transient unit (lindesk-run-NAME-<id>.scope). Default: the command's name.
    #[arg(long)]
    pub name: Option<String>,
    /// The command and its arguments. Use `--` before it if it starts with `-`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true, value_name = "COMMAND")]
    pub command: Vec<OsString>,
}

impl RunArgs {
    pub fn class(&self) -> Option<QosClass> {
        if self.batch {
            Some(QosClass::Batch)
        } else if self.background {
            Some(QosClass::Background)
        } else if self.interactive {
            Some(QosClass::Interactive)
        } else {
            self.class.map(Into::into)
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum PluginCmd {
    /// Scaffold a working plugin with tests.
    #[command(
        long_about = "Write a new plugin directory: manifest, source, vendored SDK (Python), a test \
                      and a README. Generated Python and shell plugins pass `plugin test` as-is.",
        after_help = "Examples:\n  lindeskctl plugin new --kind classifier --lang python steam-games\n  lindeskctl plugin new --kind policy --lang rust on-battery --dir ~/src\n  lindeskctl plugin new --kind sink --lang shell notify-on-red"
    )]
    New {
        /// Plugin kind.
        #[arg(long, short, value_enum)]
        kind: KindArg,
        /// Language of the template.
        #[arg(long, short, value_enum, default_value_t = LangArg::Python)]
        lang: LangArg,
        /// Plugin name: lowercase letters, digits and '-'.
        name: String,
        /// Parent directory; the plugin goes into DIR/NAME.
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },

    /// Run the protocol conformance suite against a plugin directory.
    #[command(
        long_about = "Spawn the plugin exactly as the host would and check the manifest, the \
                      handshake, canned calls for every declared kind within their timeouts, \
                      robustness to unknown methods and bad params, stdout hygiene and clean \
                      shutdown. Nothing is applied to the system. Exit status 1 on any failure.",
        after_help = "Examples:\n  lindeskctl plugin test .\n  lindeskctl plugin test ./on-battery --timeout-scale 3\n  lindeskctl plugin test . --json | jq '.checks[] | select(.ok | not)'"
    )]
    Test {
        /// Plugin directory (or its plugin.toml).
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Multiply every timeout (slow CI machines: 2-5).
        #[arg(long, default_value_t = 1.0)]
        timeout_scale: f64,
    },

    /// List built-in and installed plugins with their health.
    #[command(after_help = "Examples:\n  lindeskctl plugin list\n  lindeskctl plugin list --system --json")]
    List {
        /// Also show directories that were skipped or rejected.
        #[arg(long, short)]
        all: bool,
    },

    /// Show a plugin's manifest, path, config, health and recent errors.
    #[command(
        after_help = "Examples:\n  lindeskctl plugin info steam-games\n  lindeskctl plugin info threshold --json"
    )]
    Info { name: String },

    /// Enable a plugin in the config and reload the daemon.
    #[command(
        long_about = "Set `[plugins.NAME] enabled = true` in ~/.config/lindesk/lindesk.toml \
                      (/etc/lindesk/lindesk.toml with --system), keeping comments and layout, then \
                      ask the daemon to reload.",
        after_help = "Examples:\n  lindeskctl plugin enable steam-games\n  sudo lindeskctl plugin enable --system on-battery"
    )]
    Enable { name: String },

    /// Disable a plugin in the config and reload the daemon.
    #[command(
        long_about = "Set `[plugins.NAME] enabled = false` in ~/.config/lindesk/lindesk.toml \
                      (/etc/lindesk/lindesk.toml with --system), keeping comments and layout, then \
                      ask the daemon to reload.",
        after_help = "Examples:\n  lindeskctl plugin disable rules\n  sudo lindeskctl plugin disable --system on-battery"
    )]
    Disable { name: String },

    /// Test, build (Rust) and install a plugin directory, then reload.
    #[command(
        long_about = "Run conformance, build Rust plugins with `cargo build --release`, and copy \
                      the plugin to ~/.local/share/lindesk/plugins/NAME (with --system, as root: \
                      /etc/lindesk/plugins/NAME, root-owned). Then ask the daemon to reload.",
        after_help = "Examples:\n  lindeskctl plugin install ./steam-games\n  sudo lindeskctl plugin install --system ./on-battery\n  lindeskctl plugin install --force .   # replace an installed copy"
    )]
    Install {
        /// Plugin directory.
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Replace an existing installation.
        #[arg(long, short)]
        force: bool,
        /// Multiply conformance timeouts.
        #[arg(long, default_value_t = 1.0)]
        timeout_scale: f64,
    },

    /// Remove an installed plugin and reload.
    #[command(
        after_help = "Examples:\n  lindeskctl plugin uninstall steam-games\n  sudo lindeskctl plugin uninstall --system on-battery"
    )]
    Uninstall { name: String },

    /// Show a plugin's log lines from the journal.
    #[command(
        long_about = "Show journal lines the daemon logged for this plugin: the plugin's stderr, \
                      its `log` notifications and the host's messages about it (restarts, \
                      timeouts, protocol errors). Reads the lindesk-session user unit, or lindeskd \
                      with --system, matching the PLUGIN field written by the daemons.",
        after_help = "Examples:\n  lindeskctl plugin logs steam-games\n  lindeskctl plugin logs -f on-battery\n  lindeskctl plugin logs --system -n 200 on-battery"
    )]
    Logs {
        name: String,
        /// Follow new lines.
        #[arg(long, short)]
        follow: bool,
        /// Number of lines to show.
        #[arg(long, short = 'n', default_value_t = 50)]
        lines: usize,
    },

    /// Ask the daemon to re-read its config and plugin directories.
    #[command(after_help = "Examples:\n  lindeskctl plugin reload\n  lindeskctl plugin reload --system")]
    Reload,

    /// Print the plugin protocol's JSON Schema.
    #[command(after_help = "Example:\n  lindeskctl plugin schema > protocol.schema.json")]
    Schema,
}

#[derive(Args, Debug)]
pub struct SimulateArgs {
    /// Candidate policy: a plugin directory, or builtin:threshold.
    #[arg(long, value_name = "DIR|builtin:threshold")]
    pub policy: String,
    /// Start of the window.
    #[arg(long, default_value = "-1h", allow_hyphen_values = true, value_name = "TIME")]
    pub from: String,
    /// End of the window.
    #[arg(long, default_value = "now", allow_hyphen_values = true, value_name = "TIME")]
    pub to: String,
    /// What to compare against: builtin:threshold or none.
    #[arg(long, default_value = "builtin:threshold", value_name = "builtin:threshold|none")]
    pub baseline: String,
    /// lindesk.toml to take [plugins.*] settings from (default: your config, or /etc with --system).
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Lake directory (default: from the config).
    #[arg(long, env = "LINDESK_TELEMETRY_DIR", value_name = "DIR")]
    pub lake: Option<PathBuf>,
    /// Maximum number of action lines to print.
    #[arg(long, default_value_t = 40)]
    pub limit: usize,
    /// Multiply the candidate plugin's timeouts.
    #[arg(long, default_value_t = 1.0)]
    pub timeout_scale: f64,
}

#[derive(Args, Debug)]
pub struct QueryArgs {
    /// The SQL statement.
    pub sql: String,
    /// Output format.
    #[arg(long, short, value_enum, default_value_t = QueryFormat::Table)]
    pub format: QueryFormat,
    /// Output file (required for parquet; otherwise written instead of stdout).
    #[arg(long, short, value_name = "FILE")]
    pub out: Option<PathBuf>,
    /// Lake directory (default: from the config).
    #[arg(long, env = "LINDESK_TELEMETRY_DIR", value_name = "DIR")]
    pub lake: Option<PathBuf>,
    /// How to read the lake's Vortex files.
    #[arg(long, value_enum, default_value_t = BackendArg::Auto, env = "LINDESK_QUERY_BACKEND")]
    pub backend: BackendArg,
}

#[derive(Args, Debug, Clone)]
pub struct LakeArg {
    /// Lake directory (default: telemetry.dir from the config, else the standard location).
    #[arg(long, env = "LINDESK_TELEMETRY_DIR", value_name = "DIR")]
    pub lake: Option<PathBuf>,
    /// How to read the lake's Vortex files.
    #[arg(long, value_enum, default_value_t = BackendArg::Auto, env = "LINDESK_QUERY_BACKEND")]
    pub backend: BackendArg,
}

#[derive(Subcommand, Debug)]
pub enum DataCmd {
    /// (Re)generate catalog.duckdb, a views-only database any DuckDB client can open.
    #[command(
        long_about = "Regenerate catalog.duckdb: one view per table over the lake's Vortex files. \
                      Needs the DuckDB vortex extension (lindeskctl data install-extension); DuckDB \
                      clients must run `INSTALL vortex; LOAD vortex;` before using the views.",
        after_help = "Example:\n  lindeskctl data catalog && duckdb -readonly ~/.local/state/lindesk/catalog.duckdb -c \"INSTALL vortex; LOAD vortex; FROM lindesk_tables\""
    )]
    Catalog(LakeArg),
    /// Merge sealed hour partitions and refresh the 1m/1h rollups.
    Compact(LakeArg),
    /// Apply retention (raw / rollup TTLs and max_size) from the config.
    Prune(LakeArg),
    /// Size, files, rows and time range per table.
    Stats(LakeArg),
    /// Delete rows matching a predicate from raw tables and rollups.
    #[command(
        long_about = "Delete every row matching PREDICATE (a SQL boolean expression) from each \
                      table whose columns it uses, raw and rollup alike, rewriting only the \
                      affected partitions. Asks for confirmation on a terminal unless --yes; \
                      without a terminal --yes is required.",
        after_help = "Examples:\n  lindeskctl data delete --where \"app_id = 'org.mozilla.firefox'\"\n  lindeskctl data delete --where \"unit like 'app-steam%'\" --since 2026-10-01 --yes\n  lindeskctl data delete --where \"kind = 'focus'\" --table events --dry-run"
    )]
    Delete {
        /// SQL boolean expression.
        #[arg(long = "where", value_name = "PREDICATE")]
        predicate: String,
        /// Only rows at or after this time (RFC 3339, YYYY-MM-DD, -7d, ...).
        #[arg(long, allow_hyphen_values = true)]
        since: Option<String>,
        /// Only rows before this time.
        #[arg(long, allow_hyphen_values = true)]
        until: Option<String>,
        /// Restrict to these tables (repeatable).
        #[arg(long = "table")]
        tables: Vec<String>,
        /// Only count matching rows.
        #[arg(long)]
        dry_run: bool,
        /// Don't ask for confirmation.
        #[arg(long, short)]
        yes: bool,
        #[command(flatten)]
        lake: LakeArg,
    },
    /// Remove all telemetry.
    Purge {
        /// Don't ask for confirmation.
        #[arg(long, short)]
        yes: bool,
        #[command(flatten)]
        lake: LakeArg,
    },
    /// Open the duckdb CLI on the catalog, read-only.
    Shell(LakeArg),
    /// Pre-install the DuckDB vortex extension (needs network once) for offline use.
    #[command(
        long_about = "Download the DuckDB `vortex` extension into lindesk's extension directory \
                      (next to the lake) so queries and the catalog use it without network access \
                      later. Without it lindesk falls back to its built-in Vortex reader.",
        after_help = "Examples:\n  lindeskctl data install-extension\n  sudo lindeskctl data install-extension --system"
    )]
    InstallExtension(LakeArg),
}

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Validate a config file (default: yours, or /etc/lindesk/lindesk.toml with --system).
    #[command(
        after_help = "Examples:\n  lindeskctl config check\n  lindeskctl config check ./lindesk.toml\n  lindeskctl config check --system"
    )]
    Check { path: Option<PathBuf> },
    /// Print the effective configuration: the commented defaults with your values applied.
    #[command(
        long_about = "Print the effective configuration: the shipped, commented defaults with your \
                      values applied. --resolved instead prints the threshold policy's tunables \
                      after resolving every \"auto\" against this machine's hardware, with the \
                      reason for each value.",
        after_help = "Examples:\n  lindeskctl config show\n  lindeskctl config show --resolved\n  lindeskctl config show --system --json"
    )]
    Show {
        path: Option<PathBuf>,
        /// Show the hardware-resolved threshold tunables and why each has its value.
        #[arg(long)]
        resolved: bool,
    },
    /// Write the commented default config file if it doesn't exist yet.
    #[command(after_help = "Examples:\n  lindeskctl config init\n  sudo lindeskctl config init --system")]
    Init,
    /// Print the path of the config file in use.
    Path,
}

#[derive(Subcommand, Debug)]
pub enum ProfileCmd {
    /// Every profile, which is active, and which match this machine now.
    #[command(
        long_about = "List the shipped and installed profiles: which one is active (●), which \
                      match this machine right now and why, their auto-selection priority and \
                      description. Asks the daemon; without one, evaluates the profiles locally \
                      and shows what lindesk would pick.",
        after_help = "Examples:\n  lindeskctl profile list\n  lindeskctl profile list --json | jq '.profiles[] | select(.matches_now)'"
    )]
    List,
    /// A profile's overlay and the threshold values it resolves to on this machine.
    #[command(
        after_help = "Examples:\n  lindeskctl profile show gaming\n  lindeskctl profile show battery --json | jq .threshold.why"
    )]
    Show {
        /// Profile name.
        name: String,
    },
    /// Pin a profile, or `auto` to let lindesk choose (a versioned change).
    #[command(
        long_about = "Set `[general] profile` through a versioned change (SetProfile), so it shows \
                      up in `lindeskctl changes list` and can be reverted. `auto` returns to \
                      automatic selection.",
        after_help = "Examples:\n  lindeskctl profile use gaming\n  lindeskctl profile use auto\n  lindeskctl profile use --system battery"
    )]
    Use {
        /// A profile name, or `auto`.
        name: String,
    },
    /// The active profile and why it was chosen.
    #[command(after_help = "Examples:\n  lindeskctl profile current\n  lindeskctl profile current --json")]
    Current,
}

/// Parse a ttl: `10m`, `1h`, or `0` for none.
pub fn parse_ttl(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s == "0" || s.eq_ignore_ascii_case("none") || s.eq_ignore_ascii_case("off") {
        return Ok(Duration::ZERO);
    }
    let d = humantime::parse_duration(s).map_err(|e| format!("{e} (examples: 10m, 1h, 0 for none)"))?;
    if d > Duration::from_secs(24 * 3600) {
        return Err("at most 24h".into());
    }
    Ok(d)
}

/// `key=value` for the ops shortcuts.
pub fn parse_kv(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.trim().is_empty() => Ok((k.trim().to_string(), v.trim().to_string())),
        _ => Err(format!("expected KEY=VALUE, got '{s}'")),
    }
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnOff {
    On,
    Off,
}

/// The ops of a changeset: a JSON file (or `-` for stdin) and/or shortcut flags.
#[derive(Args, Debug, Clone, Default)]
pub struct OpsArgs {
    /// A JSON file of ops (`[{"op": "sysctl", ...}]` or an envelope), or `-` for stdin.
    #[arg(value_name = "OPS.json|-")]
    pub file: Option<String>,
    /// Set a config key: `plugins.threshold.config.epoch=400ms` (user config; /etc with --system).
    #[arg(long = "set", value_name = "KEY=VALUE", value_parser = parse_kv)]
    pub set: Vec<(String, String)>,
    /// Remove a config key (back to its default).
    #[arg(long = "unset", value_name = "KEY")]
    pub unset: Vec<String>,
    /// Set an allow-listed sysctl: `vm.swappiness=10` (system scope).
    #[arg(long = "sysctl", value_name = "KEY=VALUE", value_parser = parse_kv)]
    pub sysctl: Vec<(String, String)>,
    /// Set a disk's I/O scheduler: `nvme0n1=mq-deadline` or `259:0=bfq` (system scope).
    #[arg(long = "scheduler", value_name = "DEVICE=SCHED", value_parser = parse_kv)]
    pub scheduler: Vec<(String, String)>,
    /// Pin a profile (or `auto`).
    #[arg(long = "profile", value_name = "NAME")]
    pub profile: Option<String>,
    /// Pin a class on units matching a glob: `app-*steam*.scope=interactive`.
    #[arg(long = "class", value_name = "UNIT=CLASS", value_parser = parse_kv)]
    pub class: Vec<(String, String)>,
    /// Turn zswap on or off (system scope).
    #[arg(long, value_enum, value_name = "on|off")]
    pub zswap: Option<OnOff>,
    /// zswap compressor (lz4, zstd, ...).
    #[arg(long, value_name = "ALG")]
    pub zswap_compressor: Option<String>,
    /// zswap pool size, % of RAM (1-50).
    #[arg(long, value_name = "PCT")]
    pub zswap_max_pool: Option<u8>,
    /// cpufreq energy_performance_preference for every CPU (high risk).
    #[arg(long, value_name = "PREF")]
    pub epp: Option<String>,
    /// cpufreq governor for every CPU (high risk).
    #[arg(long, value_name = "GOV")]
    pub governor: Option<String>,
    /// Also persist sysctl/scheduler ops across reboots (/etc/sysctl.d, a udev rule).
    #[arg(long)]
    pub persist: bool,
}

#[derive(Subcommand, Debug)]
pub enum ChangesCmd {
    /// The change history: id, when, who, scope, risk, status, ttl left, description.
    #[command(
        after_help = "Examples:\n  lindeskctl changes list\n  lindeskctl changes list --system -n 50\n  lindeskctl changes list --json | jq '.[] | select(.status == \"applied\")'"
    )]
    List {
        /// How many changesets (newest first).
        #[arg(long, short = 'n', default_value_t = 20)]
        limit: u32,
    },
    /// One changeset: its ops with the values before and after.
    #[command(after_help = "Examples:\n  lindeskctl changes show 7\n  lindeskctl changes show 7 --json")]
    Show { id: u64 },
    /// What changed between two versions: their ops and the config file diff.
    #[command(
        after_help = "Examples:\n  lindeskctl changes diff 4 7\n  lindeskctl changes diff 4   # up to the latest"
    )]
    Diff {
        a: u64,
        /// Default: the latest version.
        b: Option<u64>,
    },
    /// Dry run: risk, a diff of the values, and any guardrail rejections. Nothing changes.
    #[command(
        long_about = "Validate a changeset with the daemon's guardrails and show what it would do: \
                      the risk (low: lindesk's config; medium: sysctl/zswap; high: I/O \
                      scheduler, cpufreq), each value before → after, the config diff, and the \
                      rejections, if any. Nothing is applied. System-scope ops (sysctl, \
                      scheduler, zswap, cpufreq) go to lindeskd automatically.",
        after_help = "Examples:\n  lindeskctl changes plan --set plugins.threshold.config.epoch=400ms\n  lindeskctl changes plan --sysctl vm.swappiness=10 --sysctl vm.page-cluster=0\n  lindeskctl changes plan ops.json\n  echo '[{\"op\":\"cpu_epp\",\"preference\":\"balance_performance\"}]' | lindeskctl changes plan -"
    )]
    Plan(OpsArgs),
    /// Plan, confirm, then apply a versioned changeset (optionally auto-reverting).
    #[command(
        long_about = "Plan the changeset, show it, ask for confirmation on a terminal (--yes skips \
                      it; without a terminal --yes is required), then apply it. The previous \
                      values are recorded, so `lindeskctl changes revert ID` undoes it.\n\n\
                      --ttl arms a dead-man switch: the change auto-reverts after that long \
                      unless you run `lindeskctl changes confirm ID`. High-risk changes \
                      (I/O scheduler, cpufreq) get --ttl 10m by default; pass --ttl 0 to make \
                      them permanent right away.",
        after_help = "Examples:\n  lindeskctl changes apply --set plugins.threshold.config.epoch=400ms --ttl 10m -m \"shorter epochs\"\n  lindeskctl changes apply --sysctl vm.swappiness=10 -m \"less swapping\"\n  lindeskctl changes apply --scheduler nvme0n1=mq-deadline --ttl 30m\n  lindeskctl changes apply ops.json --yes"
    )]
    Apply {
        #[command(flatten)]
        ops: OpsArgs,
        /// Auto-revert after this long unless confirmed (`10m`, `1h`; `0` = never).
        #[arg(long, value_parser = parse_ttl, value_name = "DURATION")]
        ttl: Option<Duration>,
        /// What the change is for (recorded in the history).
        #[arg(long = "message", short = 'm', value_name = "TEXT")]
        message: Option<String>,
        /// Don't ask for confirmation.
        #[arg(long, short)]
        yes: bool,
        /// Apply directly to the change store instead of through the daemon (root for
        /// system scope; recovery and tests). There is no auto-revert without a daemon.
        #[arg(long)]
        offline: bool,
        #[command(flatten)]
        local: LocalStore,
    },
    /// Keep an applied change: cancels its auto-revert.
    #[command(after_help = "Example:\n  lindeskctl changes confirm 7")]
    Confirm { id: u64 },
    /// Undo a change (recorded as a new changeset; history is append-only).
    #[command(after_help = "Examples:\n  lindeskctl changes revert 7\n  lindeskctl changes revert --system 3")]
    Revert { id: u64 },
    /// Undo every change made after a version, newest first.
    #[command(
        long_about = "Revert every changeset after VERSION that is still in effect, newest first, \
                      which returns lindesk's config and the tunables it changed to how they were \
                      at that version. Asks for confirmation on a terminal unless --yes.",
        after_help = "Examples:\n  lindeskctl changes revert-to 4\n  lindeskctl changes revert-to 0 --yes   # undo everything"
    )]
    RevertTo {
        /// Keep this version; undo everything after it.
        #[arg(value_name = "VERSION")]
        to: u64,
        /// Don't ask for confirmation.
        #[arg(long, short)]
        yes: bool,
    },
    /// Undo every change lindesk ever made in a scope, without a daemon (uninstall).
    #[command(
        long_about = "Open the change store directly (no daemon needed: run it after stopping \
                      lindesk), roll back interrupted changesets, and revert every changeset \
                      still in effect: sysctls, I/O schedulers, zswap, EPP/governor, \
                      conflicting services and config edits go back to the values recorded \
                      before lindesk changed them, and persisted sysctl.d/udev lines are \
                      removed. System scope needs root. The uninstaller runs this.",
        after_help = "Examples:\n  sudo lindeskctl changes revert-all --scope system --yes\n  lindeskctl changes revert-all --scope user"
    )]
    RevertAll {
        /// Which store: the system one (/var/lib/lindesk/changes) or yours.
        #[arg(long, value_enum, default_value_t = StoreScope::System)]
        scope: StoreScope,
        /// Don't ask for confirmation.
        #[arg(long, short)]
        yes: bool,
        #[command(flatten)]
        local: LocalStore,
    },
}

#[derive(Args, Debug, Default)]
pub struct OnArgs {
    /// Disable conflicting priority daemons without asking (a revertible change).
    #[arg(long, conflicts_with = "keep_conflicts")]
    pub disable_conflicts: bool,
    /// Leave conflicting daemons alone.
    #[arg(long)]
    pub keep_conflicts: bool,
    /// As root: also start every logged-in user's session agent now.
    #[arg(long)]
    pub all_users: bool,
    /// Don't ask.
    #[arg(long, short)]
    pub yes: bool,
    /// How long to wait for the daemons to answer, seconds.
    #[arg(long, default_value_t = 30, hide = true)]
    pub wait: u64,
}

#[derive(Args, Debug, Default)]
pub struct OffArgs {
    /// Leave conflicting daemons as they are now (default: back to their install-time state).
    #[arg(long)]
    pub keep_conflicts: bool,
    /// As root: also stop every logged-in user's session agent now.
    #[arg(long)]
    pub all_users: bool,
    /// Don't ask.
    #[arg(long, short)]
    pub yes: bool,
}

/// A change store scope.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StoreScope {
    #[default]
    System,
    User,
}

/// Where an offline change-store operation looks (hidden: fixtures and tests).
#[derive(Args, Debug, Clone, Default)]
pub struct LocalStore {
    /// Change store directory (default: /var/lib/lindesk/changes, or
    /// $XDG_STATE_HOME/lindesk/changes for user scope).
    #[arg(long, value_name = "DIR", hide = true)]
    pub store: Option<PathBuf>,
    /// Find /proc, /sys and /etc under this directory instead of / (fixture trees).
    #[arg(long, value_name = "DIR", hide = true)]
    pub root: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum SetupCmd {
    /// Other priority daemons (system76-scheduler, ananicy(-cpp), uresourced): show,
    /// disable, restore.
    #[command(
        long_about = "Without options: the state of every conflicting daemon installed here, the \
                      install-time snapshot, and which ones lindesk disabled. --disable stops and \
                      disables the running/enabled ones through a versioned change (`lindeskctl \
                      changes list` shows it; uninstall reverts it); --restore reverts those \
                      changes; --reconcile compares with the install-time snapshot \
                      (/var/lib/lindesk/install-state.json) and offers to put back units changed \
                      outside lindesk. Changing anything needs root. Why they conflict: \
                      docs/src/guide/conflicts.md.",
        after_help = "Examples:\n  lindeskctl setup conflicts\n  sudo lindeskctl setup conflicts --disable\n  sudo lindeskctl setup conflicts --restore\n  sudo lindeskctl setup conflicts --reconcile --restore-conflicts --yes"
    )]
    Conflicts {
        /// Stop and disable the conflicting daemons (a revertible change).
        #[arg(long, conflicts_with_all = ["restore", "reconcile"])]
        disable: bool,
        /// Revert what --disable did.
        #[arg(long, conflicts_with = "reconcile")]
        restore: bool,
        /// Compare with the install-time snapshot and offer to restore differences.
        #[arg(long)]
        reconcile: bool,
        /// With --reconcile: restore every difference without asking.
        #[arg(long, requires = "reconcile", conflicts_with = "keep_current")]
        restore_conflicts: bool,
        /// With --reconcile: keep the current state (the default without a terminal).
        #[arg(long, requires = "reconcile")]
        keep_current: bool,
        /// Don't ask for confirmation.
        #[arg(long, short)]
        yes: bool,
        /// Recorded as the changeset's origin.
        #[arg(long, value_enum, default_value_t = SetupOrigin::Cli, hide = true)]
        origin: SetupOrigin,
        /// State directory (default /var/lib/lindesk; fixtures and tests).
        #[arg(long, value_name = "DIR", hide = true)]
        lib_dir: Option<PathBuf>,
    },
    /// Record the install-time snapshot (install.sh and the package scripts run this).
    #[command(hide = true)]
    Snapshot {
        /// script, deb, rpm or arch.
        #[arg(long, default_value = "script")]
        method: String,
        #[arg(long, value_name = "DIR", hide = true)]
        lib_dir: Option<PathBuf>,
    },
}

/// Origin recorded by `setup conflicts`.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SetupOrigin {
    #[default]
    Cli,
    Install,
}

/// MCP clients `lindesk-mcp config` knows.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum McpClient {
    #[default]
    ClaudeCode,
    ClaudeDesktop,
    Cursor,
    Generic,
}

impl McpClient {
    pub fn as_str(self) -> &'static str {
        match self {
            McpClient::ClaudeCode => "claude-code",
            McpClient::ClaudeDesktop => "claude-desktop",
            McpClient::Cursor => "cursor",
            McpClient::Generic => "generic",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum McpCmd {
    /// Run lindesk-mcp (stdio by default; --listen for streamable HTTP).
    #[command(
        disable_help_flag = true,
        after_help = "Examples:\n  lindeskctl mcp serve\n  lindeskctl mcp serve --listen unix:$XDG_RUNTIME_DIR/lindesk/mcp.sock"
    )]
    Serve {
        /// Arguments for lindesk-mcp.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, value_name = "ARGS")]
        args: Vec<OsString>,
    },
    /// Print the snippet that registers lindesk-mcp with an MCP client.
    #[command(
        after_help = "Examples:\n  lindeskctl mcp config\n  lindeskctl mcp config --client claude-desktop\n  lindeskctl mcp config --client cursor --listen 127.0.0.1:7457"
    )]
    Config {
        /// The MCP client.
        #[arg(long, value_enum, default_value_t = McpClient::ClaudeCode)]
        client: McpClient,
        /// Describe a running HTTP endpoint (e.g. 127.0.0.1:7457) instead of stdio.
        #[arg(long, value_name = "ADDR")]
        listen: Option<String>,
    },
    /// Is lindesk-mcp installed, is the user socket active, and how to connect a client.
    Status,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn clap_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("lindeskctl").chain(args.iter().copied()))
    }

    #[test]
    fn globals_anywhere() {
        let c = parse(&["status", "--json", "--system"]).unwrap();
        assert!(c.global.json && c.global.system);
        let c = parse(&["doctor", "--preflight", "--quiet"]).unwrap();
        assert!(matches!(c.cmd, Some(Cmd::Doctor { preflight: true })) && c.global.quiet);
        let c = parse(&["--color", "never", "-q", "doctor"]).unwrap();
        assert_eq!(c.global.color, ColorWhen::Never);
        assert!(c.global.quiet);
    }

    #[test]
    fn run_takes_trailing_command() {
        let c = parse(&["run", "--batch", "make", "-j32"]).unwrap();
        let Some(Cmd::Run(r)) = c.cmd else { panic!() };
        assert_eq!(r.class(), Some(QosClass::Batch));
        assert_eq!(r.command, vec![OsString::from("make"), OsString::from("-j32")]);
        let c = parse(&["run", "--class", "critical", "--name", "obs", "--", "obs", "--verbose"]).unwrap();
        let Some(Cmd::Run(r)) = c.cmd else { panic!() };
        assert_eq!(r.class(), Some(QosClass::Critical));
        assert_eq!(r.name.as_deref(), Some("obs"));
        assert_eq!(r.command.len(), 2);
        assert!(parse(&["run", "--batch", "--background", "x"]).is_err(), "classes conflict");
        assert!(parse(&["run", "--batch"]).is_err(), "command required");
        let c = parse(&["run", "ls"]).unwrap();
        let Some(Cmd::Run(r)) = c.cmd else { panic!() };
        assert_eq!(r.class(), None);
    }

    #[test]
    fn plugin_subcommands() {
        let c = parse(&["plugin", "new", "--kind", "classifier", "--lang", "python", "steam-games"]).unwrap();
        let Some(Cmd::Plugin(PluginCmd::New { kind, lang, name, dir })) = c.cmd else { panic!() };
        assert_eq!((kind, lang, name.as_str(), dir), (KindArg::Classifier, LangArg::Python, "steam-games", ".".into()));
        assert!(parse(&["plugin", "new", "--kind", "nope", "x"]).is_err());
        let c = parse(&["plugin", "logs", "-f", "-n", "5", "x"]).unwrap();
        assert!(matches!(c.cmd, Some(Cmd::Plugin(PluginCmd::Logs { follow: true, lines: 5, .. }))));
        let c = parse(&["plugin", "test"]).unwrap();
        assert!(matches!(c.cmd, Some(Cmd::Plugin(PluginCmd::Test { .. }))));
    }

    #[test]
    fn simulate_defaults_and_relative_times() {
        let c = parse(&["simulate", "--policy", "builtin:threshold", "--from", "-2h", "--to", "-1h"]).unwrap();
        let Some(Cmd::Simulate(s)) = c.cmd else { panic!() };
        assert_eq!((s.from.as_str(), s.to.as_str(), s.baseline.as_str()), ("-2h", "-1h", "builtin:threshold"));
        assert!(parse(&["simulate"]).is_err(), "--policy is required");
    }

    #[test]
    fn external_and_bench_passthrough() {
        let c = parse(&["frobnicate", "--x", "1"]).unwrap();
        let Some(Cmd::External(v)) = c.cmd else { panic!() };
        assert_eq!(v, vec![OsString::from("frobnicate"), OsString::from("--x"), OsString::from("1")]);
        let c = parse(&["bench", "compare", "--help"]).unwrap();
        let Some(Cmd::Bench { args }) = c.cmd else { panic!() };
        assert_eq!(args, vec![OsString::from("compare"), OsString::from("--help")]);
    }

    #[test]
    fn bare_invocation_is_allowed() {
        let c = parse(&[]).unwrap();
        assert!(c.cmd.is_none());
        let c = parse(&["--json"]).unwrap();
        assert!(c.cmd.is_none() && c.global.json);
    }

    #[test]
    fn watch_interval() {
        let c = parse(&["watch", "--interval", "250ms", "-n", "3"]).unwrap();
        assert!(
            matches!(c.cmd, Some(Cmd::Watch { interval: Some(i), count: Some(3), plain: false }) if i == Duration::from_millis(250))
        );
        let c = parse(&["events", "-f", "-k", "level", "--kind", "action"]).unwrap();
        assert!(matches!(c.cmd, Some(Cmd::Events { follow: true, ref kinds, lines: 50 }) if kinds.len() == 2));
        assert!(parse(&["watch", "--interval", "0s"]).is_err());
        assert!(parse(&["watch", "--interval", "soon"]).is_err());
    }

    #[test]
    fn data_and_query() {
        let c = parse(&["data", "delete", "--where", "app_id = 'x'", "--since", "-7d", "--yes"]).unwrap();
        assert!(matches!(c.cmd, Some(Cmd::Data(DataCmd::Delete { yes: true, .. }))));
        let c = parse(&["query", "select 1", "--format", "csv"]).unwrap();
        assert!(matches!(c.cmd, Some(Cmd::Query(QueryArgs { format: QueryFormat::Csv, .. }))));
        assert!(parse(&["data", "purge"]).is_ok());
        let c = parse(&["query", "select 1", "--backend", "native"]).unwrap();
        assert!(matches!(c.cmd, Some(Cmd::Query(QueryArgs { backend: BackendArg::Native, .. }))));
        let c = parse(&["data", "stats", "--backend", "extension"]).unwrap();
        assert!(matches!(c.cmd, Some(Cmd::Data(DataCmd::Stats(LakeArg { backend: BackendArg::Extension, .. })))));
        assert!(matches!(
            parse(&["data", "install-extension"]).unwrap().cmd,
            Some(Cmd::Data(DataCmd::InstallExtension(_)))
        ));
        assert!(parse(&["data", "stats", "--backend", "magic"]).is_err());
    }

    #[test]
    fn profile_subcommands() {
        assert!(matches!(parse(&["profile", "list"]).unwrap().cmd, Some(Cmd::Profile(ProfileCmd::List))));
        let c = parse(&["profile", "use", "auto", "--system"]).unwrap();
        assert!(matches!(c.cmd, Some(Cmd::Profile(ProfileCmd::Use { ref name })) if name == "auto") && c.global.system);
        assert!(matches!(
            parse(&["profile", "show", "gaming"]).unwrap().cmd,
            Some(Cmd::Profile(ProfileCmd::Show { .. }))
        ));
        assert!(matches!(
            parse(&["profile", "current", "--json"]).unwrap().cmd,
            Some(Cmd::Profile(ProfileCmd::Current))
        ));
        assert!(parse(&["profile", "use"]).is_err(), "name required");
    }

    #[test]
    fn changes_shortcuts() {
        let c = parse(&[
            "changes",
            "apply",
            "--set",
            "plugins.threshold.config.epoch=400ms",
            "--ttl",
            "10m",
            "-m",
            "shorter epochs",
        ])
        .unwrap();
        let Some(Cmd::Changes(ChangesCmd::Apply { ops, ttl, message, yes, .. })) = c.cmd else { panic!() };
        assert_eq!(ops.set, vec![("plugins.threshold.config.epoch".to_string(), "400ms".to_string())]);
        assert_eq!(ttl, Some(Duration::from_secs(600)));
        assert_eq!(message.as_deref(), Some("shorter epochs"));
        assert!(!yes);
        let c =
            parse(&["change", "plan", "--sysctl", "vm.swappiness=10", "--scheduler", "nvme0n1=mq-deadline"]).unwrap();
        let Some(Cmd::Changes(ChangesCmd::Plan(ops))) = c.cmd else { panic!() };
        assert_eq!(ops.sysctl[0], ("vm.swappiness".into(), "10".into()));
        assert_eq!(ops.scheduler[0], ("nvme0n1".into(), "mq-deadline".into()));
        let c = parse(&["changes", "plan", "-"]).unwrap();
        assert!(matches!(c.cmd, Some(Cmd::Changes(ChangesCmd::Plan(OpsArgs { file: Some(ref f), .. }))) if f == "-"));
        let c = parse(&["changes", "apply", "ops.json", "--ttl", "0", "-y"]).unwrap();
        assert!(matches!(c.cmd, Some(Cmd::Changes(ChangesCmd::Apply { ttl: Some(d), yes: true, .. })) if d.is_zero()));
        assert!(parse(&["changes", "apply", "--sysctl", "vm.swappiness"]).is_err(), "KEY=VALUE");
        assert!(parse(&["changes", "apply", "--ttl", "48h", "--sysctl", "a=b"]).is_err(), "ttl capped");
        assert!(matches!(
            parse(&["changes", "list", "-n", "5"]).unwrap().cmd,
            Some(Cmd::Changes(ChangesCmd::List { limit: 5 }))
        ));
        assert!(matches!(
            parse(&["changes", "diff", "4"]).unwrap().cmd,
            Some(Cmd::Changes(ChangesCmd::Diff { a: 4, b: None }))
        ));
        assert!(matches!(
            parse(&["changes", "revert-to", "3", "--yes"]).unwrap().cmd,
            Some(Cmd::Changes(ChangesCmd::RevertTo { to: 3, yes: true }))
        ));
        assert!(matches!(
            parse(&["changes", "confirm", "7"]).unwrap().cmd,
            Some(Cmd::Changes(ChangesCmd::Confirm { id: 7 }))
        ));
        assert!(parse(&["changes", "confirm", "seven"]).is_err());
    }

    #[test]
    fn hardware_calibrate_swap_mcp() {
        assert!(matches!(
            parse(&["calibrate", "--quick", "--force"]).unwrap().cmd,
            Some(Cmd::Calibrate { quick: true, force: true })
        ));
        assert!(matches!(parse(&["hardware", "--json"]).unwrap().cmd, Some(Cmd::Hardware)));
        assert!(matches!(parse(&["swap"]).unwrap().cmd, Some(Cmd::Swap)));
        assert!(matches!(
            parse(&["changes", "revert-all", "--scope", "user", "--yes"]).unwrap().cmd,
            Some(Cmd::Changes(ChangesCmd::RevertAll { scope: StoreScope::User, yes: true, .. }))
        ));
        assert!(matches!(
            parse(&["setup", "conflicts", "--disable", "--yes"]).unwrap().cmd,
            Some(Cmd::Setup(SetupCmd::Conflicts { disable: true, yes: true, .. }))
        ));
        assert!(parse(&["setup", "conflicts", "--disable", "--restore"]).is_err());
        assert!(matches!(
            parse(&["pause", "--for", "10m"]).unwrap().cmd,
            Some(Cmd::Pause { duration: Some(d) }) if d == Duration::from_secs(600)
        ));
        assert!(matches!(parse(&["resume"]).unwrap().cmd, Some(Cmd::Resume)));
        assert!(matches!(
            parse(&["off", "--keep-conflicts", "--yes"]).unwrap().cmd,
            Some(Cmd::Off(OffArgs { keep_conflicts: true, yes: true, all_users: false }))
        ));
        assert!(parse(&["on", "--disable-conflicts", "--keep-conflicts"]).is_err());
        assert!(parse(&["setup", "conflicts", "--restore-conflicts"]).is_err(), "needs --reconcile");
        assert!(matches!(
            parse(&["setup", "conflicts", "--reconcile", "--keep-current"]).unwrap().cmd,
            Some(Cmd::Setup(SetupCmd::Conflicts { reconcile: true, keep_current: true, .. }))
        ));
        assert!(matches!(parse(&["mcp"]).unwrap().cmd, Some(Cmd::Mcp { cmd: None })));
        let c = parse(&["mcp", "config", "--client", "claude-desktop"]).unwrap();
        assert!(matches!(c.cmd, Some(Cmd::Mcp { cmd: Some(McpCmd::Config { client: McpClient::ClaudeDesktop, .. }) })));
        let c = parse(&["mcp", "serve", "--listen", "127.0.0.1:7457"]).unwrap();
        let Some(Cmd::Mcp { cmd: Some(McpCmd::Serve { args }) }) = c.cmd else { panic!() };
        assert_eq!(args, vec![OsString::from("--listen"), OsString::from("127.0.0.1:7457")]);
        assert!(matches!(parse(&["mcp", "status"]).unwrap().cmd, Some(Cmd::Mcp { cmd: Some(McpCmd::Status) })));
        assert!(parse(&["mcp", "config", "--client", "emacs"]).is_err());
    }
}
