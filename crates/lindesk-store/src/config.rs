//! Configuration of the `parquet-store` sink (it writes Vortex files; the name is kept for
//! config compatibility), parsed from its JSON `[config]` table.
//!
//! ```toml
//! [plugins.parquet-store.config]
//! dir = "/var/lib/lindesk/telemetry"   # default depends on privilege
//! flush = "60s"                        # humantime duration
//! mode = "system"                      # "user" (0700/0600), "system" (0750/0640) or "0750"
//! max_buffer_rows = 50000
//! queue_rows = 100000                  # async queue; oldest rows dropped beyond it
//! flush_timeout = "5s"                 # how long flush/shutdown wait for the writer
//! ```

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use crate::{AsyncOptions, StoreError, StoreOptions, layout};

/// Parsed sink configuration. Every field is optional.
#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct SinkConfig {
    /// Lake root.
    pub dir: Option<PathBuf>,
    /// Flush interval, e.g. `"60s"`.
    pub flush: Option<String>,
    /// Permission preset (`"user"`, `"system"`) or an octal directory mode (`"0750"`).
    pub mode: Option<String>,
    /// See [`StoreOptions::max_buffer_rows`].
    pub max_buffer_rows: Option<usize>,
    /// See [`AsyncOptions::queue_rows`](crate::AsyncOptions::queue_rows).
    pub queue_rows: Option<usize>,
    /// See [`AsyncOptions::flush_timeout`](crate::AsyncOptions::flush_timeout), e.g. `"5s"`.
    pub flush_timeout: Option<String>,
}

impl SinkConfig {
    /// Parses the sink's JSON config; `null` yields the defaults. Unknown keys are ignored.
    pub fn from_json(v: &serde_json::Value) -> Result<SinkConfig, StoreError> {
        if v.is_null() {
            return Ok(SinkConfig::default());
        }
        serde_json::from_value(v.clone()).map_err(|e| StoreError::Config(e.to_string()))
    }

    /// The lake root and writer options, with defaults for the system or user store.
    pub fn resolve(&self, system: bool) -> Result<(PathBuf, StoreOptions), StoreError> {
        let mut opts = match self.mode.as_deref() {
            None => {
                if system {
                    StoreOptions::system()
                } else {
                    StoreOptions::user()
                }
            }
            Some("user") => StoreOptions::user(),
            Some("system") => StoreOptions::system(),
            Some(octal) => {
                let dir_mode = u32::from_str_radix(octal.trim_start_matches("0o"), 8)
                    .ok()
                    .filter(|m| *m <= 0o7777 && m & 0o700 == 0o700)
                    .ok_or_else(|| {
                        StoreError::Config(format!("invalid mode '{octal}' (user|system|octal like 0750)"))
                    })?;
                // Files get the directory's read/write bits, never execute or world write.
                let file_mode = dir_mode & 0o664;
                StoreOptions { dir_mode, file_mode, ..StoreOptions::user() }
            }
        };
        if let Some(f) = &self.flush {
            opts.flush_interval =
                humantime::parse_duration(f).map_err(|e| StoreError::Config(format!("invalid flush '{f}': {e}")))?;
            if opts.flush_interval < Duration::from_secs(1) {
                return Err(StoreError::Config("flush interval must be at least 1s".into()));
            }
        }
        if let Some(n) = self.max_buffer_rows {
            opts.max_buffer_rows = n.max(1);
        }
        let dir = self.dir.clone().unwrap_or_else(|| layout::default_root(system));
        Ok((dir, opts))
    }

    /// Options for the asynchronous writer (no thread-init hook).
    pub fn async_options(&self) -> Result<AsyncOptions, StoreError> {
        let mut a = AsyncOptions::default();
        if let Some(n) = self.queue_rows {
            a.queue_rows = n.max(1);
        }
        if let Some(t) = &self.flush_timeout {
            a.flush_timeout = humantime::parse_duration(t)
                .map_err(|e| StoreError::Config(format!("invalid flush_timeout '{t}': {e}")))?;
        }
        Ok(a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modes_and_durations() {
        let c = SinkConfig::from_json(&serde_json::json!({"dir": "/x", "flush": "30s", "mode": "0750", "other": 1}))
            .unwrap();
        let (dir, o) = c.resolve(false).unwrap();
        assert_eq!(dir, PathBuf::from("/x"));
        assert_eq!(o.flush_interval, Duration::from_secs(30));
        assert_eq!((o.dir_mode, o.file_mode), (0o750, 0o640));
        let (_, o) = SinkConfig::default().resolve(true).unwrap();
        assert_eq!(o, StoreOptions::system());
        assert!(SinkConfig::from_json(&serde_json::json!({"mode": "0644"})).unwrap().resolve(false).is_err());
        assert!(SinkConfig::from_json(&serde_json::json!({"flush": "soon"})).unwrap().resolve(false).is_err());
        let a = SinkConfig::from_json(&serde_json::json!({"queue_rows": 7, "flush_timeout": "250ms"}))
            .unwrap()
            .async_options()
            .unwrap();
        assert_eq!((a.queue_rows, a.flush_timeout), (7, Duration::from_millis(250)));
    }
}
