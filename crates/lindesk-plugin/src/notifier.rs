//! Plugin → host notifications (`log`, `metric`, `event`, `samples`), safe from any thread.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use lindesk_plugin_api::protocol::{LogLevel, LogParams, Notification, methods};
use lindesk_plugin_api::{Event, Metric, Sample, TimestampMs};
use serde::Serialize;

/// Serialises every line written to the real stdout, so responses from the main loop and
/// notifications from adapter threads never interleave.
static STDOUT: Mutex<()> = Mutex::new(());

#[derive(Clone)]
enum Output {
    Stdout,
    Capture(Arc<Mutex<Vec<String>>>),
}

/// Sends notifications to the host. Cheap to clone; every method is thread-safe and
/// never panics (write errors are ignored: the host is gone and the plugin will see EOF).
///
/// Get one with [`crate::notifier()`] anywhere in a plugin, or receive one through
/// [`crate::Server::with_notifier`] (which also works under [`crate::testing::Harness`]).
#[derive(Clone)]
pub struct Notifier {
    out: Output,
}

impl std::fmt::Debug for Notifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.out {
            Output::Stdout => "Notifier(stdout)",
            Output::Capture(_) => "Notifier(capture)",
        })
    }
}

impl Default for Notifier {
    fn default() -> Self {
        Notifier::stdout()
    }
}

impl Notifier {
    /// A notifier writing to the process stdout (the protocol stream).
    pub fn stdout() -> Self {
        Notifier { out: Output::Stdout }
    }

    /// A notifier that records lines in memory (used by the test harness).
    pub(crate) fn capture() -> (Self, Arc<Mutex<Vec<String>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        (Notifier { out: Output::Capture(buf.clone()) }, buf)
    }

    pub(crate) fn write_line(&self, line: &str) -> io::Result<()> {
        match &self.out {
            Output::Stdout => {
                let _guard = STDOUT.lock().unwrap_or_else(|e| e.into_inner());
                let mut out = io::stdout().lock();
                out.write_all(line.as_bytes())?;
                out.write_all(b"\n")?;
                out.flush()
            }
            Output::Capture(buf) => {
                buf.lock().unwrap_or_else(|e| e.into_inner()).push(line.to_string());
                Ok(())
            }
        }
    }

    /// Send an arbitrary notification.
    pub fn notify(&self, method: &str, params: impl Serialize) {
        if let Ok(line) = serde_json::to_string(&Notification::new(method, params)) {
            let _ = self.write_line(&line);
        }
    }

    /// Log a line; the host forwards it to the journal tagged with the plugin name.
    pub fn log(&self, level: LogLevel, message: impl Into<String>) {
        self.notify(methods::LOG, LogParams { level, message: message.into() });
    }

    /// `log(LogLevel::Debug, ..)`.
    pub fn debug(&self, message: impl Into<String>) {
        self.log(LogLevel::Debug, message);
    }

    /// `log(LogLevel::Info, ..)`.
    pub fn info(&self, message: impl Into<String>) {
        self.log(LogLevel::Info, message);
    }

    /// `log(LogLevel::Warn, ..)`.
    pub fn warn(&self, message: impl Into<String>) {
        self.log(LogLevel::Warn, message);
    }

    /// `log(LogLevel::Error, ..)`.
    pub fn error(&self, message: impl Into<String>) {
        self.log(LogLevel::Error, message);
    }

    /// Record a numeric metric (lands in the `metrics` table). The host fills in the plugin name.
    pub fn metric<K, V>(&self, name: impl Into<String>, value: f64, labels: impl IntoIterator<Item = (K, V)>)
    where
        K: Into<String>,
        V: Into<String>,
    {
        let labels: BTreeMap<String, String> = labels.into_iter().map(|(k, v)| (k.into(), v.into())).collect();
        self.notify(methods::METRIC, Metric { ts: now_ms(), name: name.into(), value, labels, plugin: String::new() });
    }

    /// Push an event (adapters: `Event::Focus`, `Event::Interaction`).
    pub fn event(&self, event: Event) {
        self.notify(methods::EVENT, event);
    }

    /// Push pressure samples outside of a `sensor.poll`. Wire format: `{"samples": [...]}`.
    pub fn samples(&self, samples: Vec<Sample>) {
        #[derive(Serialize)]
        struct Params {
            samples: Vec<Sample>,
        }
        self.notify(methods::SAMPLES, Params { samples });
    }
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> TimestampMs {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as TimestampMs)
        .unwrap_or(0)
}
