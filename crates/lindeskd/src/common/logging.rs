//! Logging: journald when started by systemd (`JOURNAL_STREAM` set), stderr otherwise.
//!
//! The filter is `--log-level` when given, else `RUST_LOG`, else `info`. Either may be a
//! full `tracing_subscriber::EnvFilter` directive such as `info,lindesk_host=debug`. Plugin
//! log lines carry a `plugin` field.

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// The filter directive to use.
pub fn directive(cli: Option<&str>, rust_log: Option<&str>) -> String {
    cli.or(rust_log).filter(|s| !s.trim().is_empty()).unwrap_or("info").to_string()
}

/// Install the global subscriber. `ident` is the syslog identifier in the journal.
pub fn init(ident: &str, cli_level: Option<&str>) {
    let rust_log = std::env::var("RUST_LOG").ok();
    let d = directive(cli_level, rust_log.as_deref());
    let filter = EnvFilter::try_new(&d).unwrap_or_else(|e| {
        eprintln!("{ident}: invalid log filter '{d}' ({e}); using 'info'");
        EnvFilter::new("info")
    });
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        match tracing_journald::layer() {
            Ok(j) => {
                let j = j.with_syslog_identifier(ident.to_string());
                let _ = tracing_subscriber::registry().with(j.with_filter(filter)).try_init();
                return;
            }
            Err(e) => eprintln!("{ident}: journald unavailable ({e}); logging to stderr"),
        }
    }
    let fmt = tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_target(false);
    let _ = tracing_subscriber::registry().with(fmt.with_filter(filter)).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence() {
        assert_eq!(directive(Some("debug"), Some("warn")), "debug");
        assert_eq!(directive(None, Some("warn")), "warn");
        assert_eq!(directive(None, None), "info");
        assert_eq!(directive(Some(" "), None), "info");
    }
}
