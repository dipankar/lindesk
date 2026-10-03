//! Probe this machine and print the doctor report.
//!
//! ```text
//! cargo run -p lindesk-sys --example probe            # text
//! cargo run -p lindesk-sys --example probe -- --json  # JSON
//! ```

use std::io::IsTerminal;

use lindesk_sys::{Env, Roots, doctor, probe};

fn main() {
    let p = probe(&Roots::host(), &Env::from_process());
    if std::env::args().any(|a| a == "--json") {
        println!("{}", doctor::render_json(&p));
    } else {
        print!("{}", doctor::render_text(&p, std::io::stdout().is_terminal()));
    }
}
