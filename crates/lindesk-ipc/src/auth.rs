//! Caller identification and polkit authorization for the system broker.

use std::collections::HashMap;

use zbus::names::BusName;
use zbus::zvariant::Value;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("D-Bus: {0}")]
    Bus(#[from] zbus::Error),
    #[error("D-Bus: {0}")]
    Fdo(#[from] zbus::fdo::Error),
    #[error("caller has no unix credentials")]
    NoCredentials,
    #[error("not authorized: {0}")]
    Denied(String),
}

/// Unix identity of a D-Bus caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caller {
    pub uid: u32,
    pub pid: Option<u32>,
}

/// Look up the uid/pid of the connection that sent a message.
pub async fn caller(conn: &zbus::Connection, sender: &BusName<'_>) -> Result<Caller, AuthError> {
    let dbus = zbus::fdo::DBusProxy::new(conn).await?;
    let creds = dbus.get_connection_credentials(sender.clone()).await?;
    let uid = creds.unix_user_id().ok_or(AuthError::NoCredentials)?;
    Ok(Caller { uid, pid: creds.process_id() })
}

/// Ask polkit whether `sender` may perform `action`. Root is always allowed.
/// `interactive` lets polkit show an authentication dialog.
pub async fn authorize(
    conn: &zbus::Connection,
    sender: &BusName<'_>,
    action: &str,
    interactive: bool,
) -> Result<(), AuthError> {
    if caller(conn, sender).await?.uid == 0 {
        return Ok(());
    }
    let mut subject_details: HashMap<&str, Value<'_>> = HashMap::new();
    subject_details.insert("name", Value::from(sender.as_str()));
    let subject = ("system-bus-name", subject_details);
    let details: HashMap<&str, &str> = HashMap::new();
    let flags: u32 = if interactive { 1 } else { 0 };
    let reply = conn
        .call_method(
            Some("org.freedesktop.PolicyKit1"),
            "/org/freedesktop/PolicyKit1/Authority",
            Some("org.freedesktop.PolicyKit1.Authority"),
            "CheckAuthorization",
            &(subject, action, details, flags, ""),
        )
        .await?;
    // polkit's AuthorizationResult is (bba{ss}): the details are strings, not variants.
    let (authorized, _challenge, _details): (bool, bool, HashMap<String, String>) = reply.body().deserialize()?;
    if authorized { Ok(()) } else { Err(AuthError::Denied(action.to_string())) }
}
