//! A small blocking client for the systemd manager's D-Bus API, shared by
//! [`super::SystemdActuator`] and [`crate::env::SysEnvironment`].
//!
//! It keeps one connection and reconnects lazily after transport errors. Unit object paths
//! are looked up with `GetUnit` on every property read and never cached: systemd answers
//! property reads on the path of an unloaded unit by loading a stub with default values,
//! which would make a vanished unit look like an unconfigured one.
//!
//! The blocking API runs zbus on its own internal tokio runtime, so these calls must not be
//! made from inside a tokio runtime thread (use a dedicated thread or `spawn_blocking`).

use std::fmt;

use zbus::blocking::Connection;

/// Deadline for one call to systemd.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
use zbus::zvariant::{DynamicType, OwnedObjectPath, OwnedValue, Value};

use crate::actuators::Bus;

/// Well-known name of the systemd manager.
pub const DESTINATION: &str = "org.freedesktop.systemd1";
/// Object path of the manager.
pub const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
/// Manager interface.
pub const MANAGER_IFACE: &str = "org.freedesktop.systemd1.Manager";
/// Interface with the properties common to all units.
pub const UNIT_IFACE: &str = "org.freedesktop.systemd1.Unit";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";
const NO_SUCH_UNIT: &str = "org.freedesktop.systemd1.NoSuchUnit";
const LOAD_FAILED: &str = "org.freedesktop.systemd1.LoadFailed";

/// Errors from [`SystemdClient`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SystemdError {
    /// The unit is not loaded (stopped and garbage-collected, or never existed).
    NoSuchUnit(String),
    /// systemd answered with a D-Bus error.
    Call { name: String, message: String },
    /// Connecting or talking to the bus failed; the connection is re-established on the
    /// next call.
    Connection(String),
    /// A property had an unexpected type.
    Type(String),
}

impl fmt::Display for SystemdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SystemdError::NoSuchUnit(u) => write!(f, "no such unit {u}"),
            SystemdError::Call { name, message } => write!(f, "{name}: {message}"),
            SystemdError::Connection(m) => write!(f, "D-Bus connection: {m}"),
            SystemdError::Type(m) => write!(f, "unexpected property type: {m}"),
        }
    }
}

impl std::error::Error for SystemdError {}

/// The D-Bus interface carrying a unit type's cgroup properties (`CPUWeight`,
/// `ControlGroup`, ...), from the unit name's suffix. `None` for unit types without a cgroup.
pub fn unit_interface(unit: &str) -> Option<&'static str> {
    Some(match unit.rsplit_once('.')?.1 {
        "service" => "org.freedesktop.systemd1.Service",
        "scope" => "org.freedesktop.systemd1.Scope",
        "slice" => "org.freedesktop.systemd1.Slice",
        "socket" => "org.freedesktop.systemd1.Socket",
        "mount" => "org.freedesktop.systemd1.Mount",
        "swap" => "org.freedesktop.systemd1.Swap",
        _ => return None,
    })
}

/// A `t` property.
pub fn as_u64(v: &Value<'_>) -> Option<u64> {
    match v {
        Value::U64(n) => Some(*n),
        Value::Value(inner) => as_u64(inner),
        _ => None,
    }
}

/// An `s` property.
pub fn as_string(v: &Value<'_>) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.as_str().to_string()),
        Value::Value(inner) => as_string(inner),
        _ => None,
    }
}

/// An `a(st)` property such as `IOReadBandwidthMax`: `(device path, value)` pairs.
pub fn as_device_values(v: &Value<'_>) -> Option<Vec<(String, u64)>> {
    match v {
        Value::Array(a) => a
            .inner()
            .iter()
            .map(|e| match e {
                Value::Structure(s) => match s.fields() {
                    [Value::Str(p), Value::U64(n)] => Some((p.as_str().to_string(), *n)),
                    _ => None,
                },
                _ => None,
            })
            .collect(),
        Value::Value(inner) => as_device_values(inner),
        _ => None,
    }
}

/// Blocking client for one systemd manager. See the [module docs](self).
pub struct SystemdClient {
    bus: Bus,
    uid: u32,
    conn: Option<Connection>,
}

impl fmt::Debug for SystemdClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemdClient")
            .field("bus", &self.bus)
            .field("uid", &self.uid)
            .field("connected", &self.conn.is_some())
            .finish()
    }
}

impl SystemdClient {
    /// A client for `bus`. `uid` locates the user bus socket when `DBUS_SESSION_BUS_ADDRESS`
    /// is unset. Nothing is connected until the first call.
    pub fn new(bus: Bus, uid: u32) -> Self {
        SystemdClient { bus, uid, conn: None }
    }

    /// The manager this client talks to.
    pub fn bus(&self) -> Bus {
        self.bus
    }

    /// True while a connection is open.
    pub fn is_connected(&self) -> bool {
        self.conn.is_some()
    }

    /// Open the connection now (it is otherwise opened by the first call).
    pub fn connect(&mut self) -> Result<&Connection, SystemdError> {
        if self.conn.is_none() {
            use zbus::blocking::connection::Builder;
            // Every call has a deadline: if systemd itself is stuck (PID 1 blocked in the
            // kernel under extreme I/O load), a call must fail, not hang the engine thread.
            let conn = match self.bus {
                Bus::System => Builder::system().and_then(|b| b.method_timeout(CALL_TIMEOUT).build()),
                Bus::User => Builder::session().and_then(|b| b.method_timeout(CALL_TIMEOUT).build()).or_else(|e| {
                    let path = format!("/run/user/{}/bus", self.uid);
                    if !std::path::Path::new(&path).exists() {
                        return Err(e);
                    }
                    Builder::address(format!("unix:path={path}").as_str())?.method_timeout(CALL_TIMEOUT).build()
                }),
            }
            .map_err(|e| SystemdError::Connection(format!("{} bus: {e}", self.bus)))?;
            self.conn = Some(conn);
        }
        Ok(self.conn.as_ref().expect("just connected"))
    }

    fn call<B>(&mut self, path: &str, iface: &str, method: &str, body: &B) -> Result<zbus::Message, SystemdError>
    where
        B: serde::Serialize + DynamicType,
    {
        let conn = self.connect()?;
        match conn.call_method(Some(DESTINATION), path, Some(iface), method, body) {
            Ok(m) => Ok(m),
            Err(zbus::Error::MethodError(name, message, _)) => {
                let message = message.unwrap_or_default();
                if name.as_str() == NO_SUCH_UNIT || name.as_str() == LOAD_FAILED {
                    Err(SystemdError::NoSuchUnit(message))
                } else {
                    Err(SystemdError::Call { name: name.to_string(), message })
                }
            }
            Err(zbus::Error::FDO(e)) => Err(SystemdError::Call { name: "fdo".into(), message: e.to_string() }),
            Err(e) => {
                // Transport trouble: start over with a fresh connection next time.
                self.conn = None;
                Err(SystemdError::Connection(e.to_string()))
            }
        }
    }

    /// `Manager.GetUnit`: the object path of a loaded unit.
    pub fn unit_path(&mut self, unit: &str) -> Result<OwnedObjectPath, SystemdError> {
        let reply = self.call(MANAGER_PATH, MANAGER_IFACE, "GetUnit", &(unit,)).map_err(|e| match e {
            SystemdError::NoSuchUnit(_) => SystemdError::NoSuchUnit(unit.to_string()),
            e => e,
        })?;
        reply.body().deserialize::<OwnedObjectPath>().map_err(|e| SystemdError::Type(e.to_string()))
    }

    /// One property of a loaded unit (`NoSuchUnit` when it isn't loaded).
    pub fn property(&mut self, unit: &str, iface: &str, name: &str) -> Result<OwnedValue, SystemdError> {
        let path = self.unit_path(unit)?;
        let reply = self.call(path.as_str(), PROPERTIES_IFACE, "Get", &(iface, name))?;
        reply.body().deserialize::<OwnedValue>().map_err(|e| SystemdError::Type(format!("{unit} {name}: {e}")))
    }

    /// A `t` property.
    pub fn property_u64(&mut self, unit: &str, iface: &str, name: &str) -> Result<u64, SystemdError> {
        let v = self.property(unit, iface, name)?;
        as_u64(&v).ok_or_else(|| SystemdError::Type(format!("{unit} {name} is not a u64: {v:?}")))
    }

    /// An `s` property.
    pub fn property_string(&mut self, unit: &str, iface: &str, name: &str) -> Result<String, SystemdError> {
        let v = self.property(unit, iface, name)?;
        as_string(&v).ok_or_else(|| SystemdError::Type(format!("{unit} {name} is not a string: {v:?}")))
    }

    /// An `a(st)` property.
    pub fn property_device_values(
        &mut self,
        unit: &str,
        iface: &str,
        name: &str,
    ) -> Result<Vec<(String, u64)>, SystemdError> {
        let v = self.property(unit, iface, name)?;
        as_device_values(&v).ok_or_else(|| SystemdError::Type(format!("{unit} {name} is not a(st): {v:?}")))
    }

    /// `Manager.SetUnitProperties(unit, runtime = true, props)`: a runtime-only change that
    /// systemd also records as a drop-in under `/run`.
    pub fn set_properties(&mut self, unit: &str, props: &[(&str, Value<'_>)]) -> Result<(), SystemdError> {
        self.call(MANAGER_PATH, MANAGER_IFACE, "SetUnitProperties", &(unit, true, props)).map(|_| ()).map_err(|e| {
            match e {
                SystemdError::NoSuchUnit(_) => SystemdError::NoSuchUnit(unit.to_string()),
                e => e,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Array;

    #[test]
    fn interfaces_by_suffix() {
        assert_eq!(unit_interface("a.service"), Some("org.freedesktop.systemd1.Service"));
        assert_eq!(unit_interface("app-gnome-x-1.scope"), Some("org.freedesktop.systemd1.Scope"));
        assert_eq!(unit_interface("user.slice"), Some("org.freedesktop.systemd1.Slice"));
        assert_eq!(unit_interface("timers.target"), None);
        assert_eq!(unit_interface("noext"), None);
    }

    #[test]
    fn value_conversions() {
        assert_eq!(as_u64(&Value::U64(7)), Some(7));
        assert_eq!(as_u64(&Value::Value(Box::new(Value::U64(u64::MAX)))), Some(u64::MAX));
        assert_eq!(as_u64(&Value::U32(7)), None);
        assert_eq!(as_string(&Value::from("/x.slice")).as_deref(), Some("/x.slice"));
        let v = Value::from(vec![("/dev/block/259:0".to_string(), 10_485_760u64)]);
        assert_eq!(as_device_values(&v), Some(vec![("/dev/block/259:0".to_string(), 10_485_760)]));
        let empty: Vec<(String, u64)> = Vec::new();
        assert_eq!(as_device_values(&Value::from(empty)), Some(vec![]));
        assert_eq!(as_device_values(&Value::Array(Array::from(vec![1u32]))), None);
    }
}
