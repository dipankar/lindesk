//! `lindeskctl`: the command-line front door to lindesk.
//!
//! Exit status: 0 success, 1 failure, 2 usage error. Every command takes `--json`,
//! `--system`, `--color` and `-q`.

mod bus;
mod cli;
mod cmd;
mod hints;
mod live;
mod paths;
mod procs;
mod telemetry;
mod ui;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::{Cli, Cmd};
use crate::ui::{Exit, Ui, Usage, paint};

fn run(cli: Cli) -> anyhow::Result<()> {
    let ui = Ui::new(&cli.global);
    let Some(command) = &cli.cmd else {
        return cmd::status::bare(&ui);
    };
    match command {
        Cmd::Doctor { preflight } => {
            if *preflight {
                cmd::doctor::preflight(&ui)
            } else {
                cmd::doctor::run(&ui)
            }
        }
        Cmd::Status => cmd::status::status(&ui),
        Cmd::Watch { interval, count, plain } => cmd::watch::watch(&ui, *interval, *count, *plain),
        Cmd::Events { follow, kinds, lines } => cmd::events::run(&ui, *follow, kinds, *lines),
        Cmd::Explain { what } => cmd::control::explain(&ui, what),
        Cmd::Run(args) => cmd::control::run(&ui, args),
        Cmd::Set { what, class } => cmd::control::set(&ui, what, (*class).into()),
        Cmd::Restore { stop } => cmd::control::restore(&ui, *stop),
        Cmd::Plugin(p) => cmd::plugin::run(&ui, p),
        Cmd::Simulate(a) => cmd::simulate::run(&ui, a),
        Cmd::Query(a) => cmd::data::query(&ui, a),
        Cmd::Data(d) => cmd::data::data(&ui, d),
        Cmd::Bench { args } => cmd::tools::bench(args),
        Cmd::Config(c) => cmd::config::run(&ui, c),
        Cmd::Profile(p) => cmd::profile::run(&ui, p),
        Cmd::Changes(c) => cmd::changes::run(&ui, c),
        Cmd::Setup(c) => cmd::setup::run(&ui, c),
        Cmd::On(a) => cmd::power::on(&ui, a),
        Cmd::Off(a) => cmd::power::off(&ui, a),
        Cmd::Pause { duration } => cmd::power::pause(&ui, *duration),
        Cmd::Resume => cmd::power::resume(&ui),
        Cmd::Calibrate { quick, force } => cmd::hw::calibrate_cmd(&ui, *quick, *force),
        Cmd::Hardware => cmd::hw::hardware_cmd(&ui),
        Cmd::Swap => cmd::hw::swap_cmd(&ui),
        Cmd::Mcp { cmd: sub } => cmd::mcp::run(&ui, sub.as_ref()),
        Cmd::Completions { shell } => {
            cmd::tools::completions(*shell, &mut std::io::stdout());
            Ok(())
        }
        Cmd::Man { dir } => cmd::tools::man_cmd(&ui, dir),
        Cmd::External(args) => cmd::tools::external(args),
    }
}

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            // --help/--version print to stdout and exit 0; real usage errors exit 2.
            let _ = e.print();
            return ExitCode::from(if e.use_stderr() { 2 } else { 0 });
        }
    };
    let err_color = Ui::new(&cli.global).err_color;
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if let Some(Exit(code)) = e.downcast_ref::<Exit>() {
                return ExitCode::from(*code);
            }
            let usage = e.downcast_ref::<Usage>().is_some();
            let mut msg = format!("{e}");
            for cause in e.chain().skip(1) {
                let c = cause.to_string();
                if !msg.contains(&c) {
                    msg.push_str(&format!("\n  caused by: {c}"));
                }
            }
            eprintln!("{}", ui::polish(&format!("{} {msg}", paint(err_color, "1;31", "error:"))));
            ExitCode::from(if usage { 2 } else { 1 })
        }
    }
}
