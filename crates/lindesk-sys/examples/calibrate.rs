//! Calibrate this machine's latency map.
//!
//! ```text
//! cargo run -p lindesk-sys --example calibrate -- --quick           # ≈3 s, text
//! cargo run -p lindesk-sys --example calibrate -- --json            # ≈15 s, JSON
//! cargo run -p lindesk-sys --example calibrate -- --force --save /tmp/cal.json
//! cargo run -p lindesk-sys --example calibrate -- --scratch /mnt/data
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use lindesk_sys::Roots;
use lindesk_sys::calibrate::{self, CalibrateOptions};

fn main() -> ExitCode {
    let mut opts = CalibrateOptions::default();
    let (mut json, mut save) = (false, None);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--quick" => opts.quick = true,
            "--force" => opts.force = true,
            "--json" => json = true,
            "--save" => save = args.next().map(PathBuf::from),
            "--scratch" => opts.scratch_dirs.extend(args.next().map(PathBuf::from)),
            _ => {
                eprintln!("usage: calibrate [--quick] [--force] [--json] [--save PATH] [--scratch DIR]");
                return ExitCode::from(2);
            }
        }
    }
    let c = match calibrate::run_detailed(&Roots::host(), &opts) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("calibrate: {e}");
            return ExitCode::FAILURE;
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&c).expect("serialises"));
    } else {
        print!("{}", calibrate::render_text(&c));
    }
    if let Some(path) = save {
        if let Err(e) = calibrate::save(&path, &c.map) {
            eprintln!("calibrate: saving {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
        eprintln!("saved {}", path.display());
    }
    ExitCode::SUCCESS
}
