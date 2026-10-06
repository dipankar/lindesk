//! A journald layer that cannot block the daemon.
//!
//! The usual way to log to the journal is a blocking datagram per record. That couples the
//! daemon to journald: when journald is stuck behind a saturated disk (exactly the situation
//! lindesk exists for) its socket queue fills, the next `send` sleeps, and the thread that
//! logged stops. For the engine thread that means a watchdog kill; during start-up it means
//! a start timeout, over and over.
//!
//! This layer sends on a non-blocking socket instead. A record journald cannot take right
//! now is dropped and counted, and the count is reported once records get through again.
//!
//! The wire format is journald's native protocol with the fields `tracing-journald` uses, so
//! existing `journalctl` filters keep working: `MESSAGE`, `PRIORITY`, `SYSLOG_IDENTIFIER`,
//! `TARGET`, `CODE_FILE`, `CODE_LINE`, and every other event or span field as
//! `<prefix>_<NAME>` (`F_PLUGIN`).

use std::fmt::{self, Write as _};
use std::io;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// journald's native-protocol socket.
pub const JOURNAL_SOCKET: &str = "/run/systemd/journal/socket";

/// See the [module docs](self).
#[derive(Debug)]
pub struct JournalLayer {
    socket: UnixDatagram,
    path: PathBuf,
    ident: String,
    prefix: String,
    /// Records journald did not take since the last one it did.
    dropped: AtomicU64,
}

impl JournalLayer {
    /// A layer logging to the system journal as `ident`. Fails when there is no journal socket.
    pub fn new(ident: &str, field_prefix: &str) -> io::Result<Self> {
        Self::to(Path::new(JOURNAL_SOCKET), ident, field_prefix)
    }

    /// A layer sending to the datagram socket at `path`.
    pub fn to(path: &Path, ident: &str, field_prefix: &str) -> io::Result<Self> {
        let socket = UnixDatagram::unbound()?;
        socket.set_nonblocking(true)?;
        // Fail now rather than drop every record later.
        std::fs::metadata(path)?;
        Ok(JournalLayer {
            socket,
            path: path.to_path_buf(),
            ident: ident.to_string(),
            prefix: field_prefix.to_string(),
            dropped: AtomicU64::new(0),
        })
    }

    fn header(&self, buf: &mut Vec<u8>, priority: u8) {
        put(buf, "PRIORITY", &[b'0' + priority]);
        put(buf, "SYSLOG_IDENTIFIER", self.ident.as_bytes());
    }

    fn send(&self, payload: &[u8]) {
        if self.socket.send_to(payload, &self.path).is_err() {
            // Full queue (journald is not reading), journald restarting, an oversized record:
            // none of them is worth a stalled thread.
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let missed = self.dropped.swap(0, Ordering::Relaxed);
        if missed > 0 {
            let mut buf = Vec::with_capacity(160);
            self.header(&mut buf, 4);
            put(
                &mut buf,
                "MESSAGE",
                format!("{missed} log records were dropped while the journal was not accepting them").as_bytes(),
            );
            if self.socket.send_to(&buf, &self.path).is_err() {
                self.dropped.fetch_add(missed, Ordering::Relaxed);
            }
        }
    }
}

/// syslog priority for a tracing level (the mapping `tracing-journald` uses).
fn priority(level: &Level) -> u8 {
    match *level {
        Level::ERROR => 3,
        Level::WARN => 4,
        Level::INFO => 5,
        Level::DEBUG => 6,
        Level::TRACE => 7,
    }
}

/// Appends one field in the length-prefixed form, which is valid for any value. The name is
/// upper-cased and anything journald would refuse in it becomes `_`.
fn put(buf: &mut Vec<u8>, name: &str, value: &[u8]) {
    buf.extend(name.bytes().map(|b| if b.is_ascii_alphanumeric() { b.to_ascii_uppercase() } else { b'_' }));
    buf.push(b'\n');
    buf.extend_from_slice(&(value.len() as u64).to_le_bytes());
    buf.extend_from_slice(value);
    buf.push(b'\n');
}

/// The fields of a span, already encoded; prepended to every event inside it.
struct SpanFields(Vec<u8>);

struct Fields<'a> {
    buf: &'a mut Vec<u8>,
    prefix: &'a str,
    /// Events: `message` is the journal's `MESSAGE`.
    message: bool,
    scratch: String,
}

impl Fields<'_> {
    fn field(&mut self, field: &Field, value: &[u8]) {
        if self.message && field.name() == "message" {
            put(self.buf, "MESSAGE", value);
        } else if self.prefix.is_empty() {
            put(self.buf, field.name(), value);
        } else {
            put(self.buf, &format!("{}_{}", self.prefix, field.name()), value);
        }
    }
}

impl Visit for Fields<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.field(field, value.as_bytes());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let mut s = std::mem::take(&mut self.scratch);
        s.clear();
        let _ = write!(s, "{value:?}");
        self.field(field, s.as_bytes());
        self.scratch = s;
    }
}

impl<S> tracing_subscriber::Layer<S> for JournalLayer
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut buf = Vec::with_capacity(128);
        put(&mut buf, "SPAN_NAME", span.name().as_bytes());
        attrs.record(&mut Fields { buf: &mut buf, prefix: &self.prefix, message: false, scratch: String::new() });
        span.extensions_mut().insert(SpanFields(buf));
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut exts = span.extensions_mut();
        if let Some(SpanFields(buf)) = exts.get_mut::<SpanFields>() {
            values.record(&mut Fields { buf, prefix: &self.prefix, message: false, scratch: String::new() });
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut buf = Vec::with_capacity(256);
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                if let Some(SpanFields(fields)) = span.extensions().get::<SpanFields>() {
                    buf.extend_from_slice(fields);
                }
            }
        }
        let meta = event.metadata();
        self.header(&mut buf, priority(meta.level()));
        put(&mut buf, "TARGET", meta.target().as_bytes());
        if let Some(file) = meta.file() {
            put(&mut buf, "CODE_FILE", file.as_bytes());
        }
        if let Some(line) = meta.line() {
            put(&mut buf, "CODE_LINE", line.to_string().as_bytes());
        }
        event.record(&mut Fields { buf: &mut buf, prefix: &self.prefix, message: true, scratch: String::new() });
        self.send(&buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};
    use tracing_subscriber::layer::SubscriberExt;

    /// Decodes one native-protocol datagram (every field length-prefixed, as [`put`] writes).
    fn decode(mut d: &[u8]) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        while !d.is_empty() {
            let nl = d.iter().position(|b| *b == b'\n').expect("field name");
            let name = String::from_utf8(d[..nl].to_vec()).unwrap();
            let len = usize::try_from(u64::from_le_bytes(d[nl + 1..nl + 9].try_into().unwrap())).unwrap();
            let value = String::from_utf8(d[nl + 9..nl + 9 + len].to_vec()).unwrap();
            assert_eq!(d[nl + 9 + len], b'\n');
            out.insert(name, value);
            d = &d[nl + 10 + len..];
        }
        out
    }

    fn journal() -> (tempfile::TempDir, UnixDatagram, JournalLayer) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("socket");
        let rx = UnixDatagram::bind(&path).unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let layer = JournalLayer::to(&path, "lindesk-test", "F").unwrap();
        (dir, rx, layer)
    }

    fn recv(rx: &UnixDatagram) -> BTreeMap<String, String> {
        let mut buf = vec![0u8; 65536];
        let n = rx.recv(&mut buf).expect("a journal record");
        decode(&buf[..n])
    }

    #[test]
    fn records_carry_the_fields_journalctl_filters_on() {
        let (_dir, rx, layer) = journal();
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            let span = tracing::info_span!("plugin", plugin = "steam-games");
            let _in = span.enter();
            tracing::warn!(restarts = 3, "crashed:\nexit 1");
        });
        let r = recv(&rx);
        assert_eq!(r["MESSAGE"], "crashed:\nexit 1", "multi-line values survive");
        assert_eq!(r["PRIORITY"], "4");
        assert_eq!(r["SYSLOG_IDENTIFIER"], "lindesk-test");
        assert_eq!(r["F_PLUGIN"], "steam-games", "span fields, as `lindeskctl plugin logs` matches them");
        assert_eq!(r["F_RESTARTS"], "3");
        assert_eq!(r["SPAN_NAME"], "plugin");
        assert!(r["TARGET"].contains("journal") && r.contains_key("CODE_LINE"));
    }

    #[test]
    fn a_journal_that_stops_reading_costs_records_not_time() {
        let (_dir, rx, layer) = journal();
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            // Nobody reads: the queue fills, and every further record must return at once.
            let t0 = Instant::now();
            for i in 0..20_000 {
                tracing::info!(i, "apply cpu.weight=50 on a unit with a fairly long name.service");
            }
            assert!(t0.elapsed() < Duration::from_secs(20), "logging blocked for {:?}", t0.elapsed());

            // journald catches up: what was queued arrives, then the count of what was not.
            rx.set_nonblocking(true).unwrap();
            let mut buf = vec![0u8; 65536];
            let mut queued = 0;
            while rx.recv(&mut buf).is_ok() {
                queued += 1;
            }
            assert!(queued > 0 && queued < 20_000, "the queue never filled ({queued} records got through)");
            let dropped = 20_000 - queued;
            rx.set_nonblocking(false).unwrap();
            tracing::info!("back");
            assert_eq!(recv(&rx)["MESSAGE"], "back");
            let notice = recv(&rx);
            assert_eq!(
                notice["MESSAGE"],
                format!("{dropped} log records were dropped while the journal was not accepting them")
            );
            assert_eq!(notice["PRIORITY"], "4");
            // The count starts over.
            tracing::info!("again");
            assert_eq!(recv(&rx)["MESSAGE"], "again");
        });
    }

    #[test]
    fn no_socket_no_layer() {
        assert!(JournalLayer::to(Path::new("/nonexistent/journal/socket"), "x", "F").is_err());
    }
}
