//! Parsing the `what` argument of `Explain` and rendering the reply.

use lindesk_host::Explanation;
use lindesk_plugin_api::{Manager, Target};
use serde_json::{Value, json};

/// What `Explain(what)` asks about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum What {
    /// `1234` or `pid:1234`: the unit owning that process.
    Pid(i32),
    /// A target in `Target`'s display format: `system:x.service`, `user/1000:app-….scope`,
    /// `cgroup:/…` (or a bare cgroup path starting with `/`).
    Target(Target),
    /// A bare unit name (`firefox.scope`); the daemon picks the manager.
    BareUnit(String),
}

fn pid(s: &str) -> Result<i32, String> {
    match s.parse::<i32>() {
        Ok(p) if p > 0 => Ok(p),
        _ => Err(format!("'{s}' is not a valid pid")),
    }
}

fn unit_name(s: &str, what: &str) -> Result<String, String> {
    if s.is_empty() || s.contains(['/', ':']) || s.chars().any(char::is_whitespace) {
        return Err(format!("'{what}': invalid unit name '{s}'"));
    }
    Ok(s.to_string())
}

/// Parse `what`. See [`What`].
pub fn parse(what: &str) -> Result<What, String> {
    let s = what.trim();
    if s.is_empty() {
        return Err("nothing to explain: pass a pid, a unit or a target".into());
    }
    if s.bytes().all(|b| b.is_ascii_digit()) {
        return pid(s).map(What::Pid);
    }
    if let Some(p) = s.strip_prefix("pid:") {
        return pid(p).map(What::Pid);
    }
    if let Some(name) = s.strip_prefix("system:") {
        return Ok(What::Target(Target::Unit { manager: Manager::System, name: unit_name(name, s)? }));
    }
    if let Some(rest) = s.strip_prefix("user/") {
        let (uid, name) = rest.split_once(':').ok_or_else(|| format!("'{s}': expected user/<uid>:<unit>"))?;
        let uid: u32 = uid.parse().map_err(|_| format!("'{s}': '{uid}' is not a uid"))?;
        return Ok(What::Target(Target::Unit { manager: Manager::User(uid), name: unit_name(name, s)? }));
    }
    if let Some(path) = s.strip_prefix("cgroup:") {
        if !path.starts_with('/') {
            return Err(format!("'{s}': cgroup paths start with '/'"));
        }
        return Ok(What::Target(Target::Cgroup { path: path.to_string() }));
    }
    if s.starts_with('/') {
        return Ok(What::Target(Target::Cgroup { path: s.to_string() }));
    }
    unit_name(s, s).map(What::BareUnit)
}

/// The `Explain` reply: the [`Explanation`] fields plus `"query"` (the original `what`) and
/// `"text"` (the human rendering `lindeskctl explain` prints).
pub fn explanation_json(query: &str, e: &Explanation) -> String {
    let mut v = serde_json::to_value(e).unwrap_or_else(|_| json!({}));
    if let Some(o) = v.as_object_mut() {
        o.insert("query".into(), Value::String(query.to_string()));
        o.insert("text".into(), Value::String(e.to_string()));
    }
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pids() {
        assert_eq!(parse("1234"), Ok(What::Pid(1234)));
        assert_eq!(parse(" 42 "), Ok(What::Pid(42)));
        assert_eq!(parse("pid:7"), Ok(What::Pid(7)));
        assert!(parse("0").is_err());
        assert!(parse("pid:-3").is_err());
        assert!(parse("99999999999").is_err());
        assert!(parse("pid:x").is_err());
    }

    #[test]
    fn targets_round_trip_display() {
        for t in [
            Target::system_unit("packagekit.service"),
            Target::user_unit(1000, "app-gnome-org.mozilla.firefox-1234.scope"),
            Target::Cgroup { path: "/user.slice/user-1000.slice".into() },
        ] {
            assert_eq!(parse(&t.to_string()), Ok(What::Target(t.clone())), "{t}");
        }
        assert_eq!(parse("/system.slice"), Ok(What::Target(Target::Cgroup { path: "/system.slice".into() })));
    }

    #[test]
    fn bare_units_and_errors() {
        assert_eq!(parse("firefox.scope"), Ok(What::BareUnit("firefox.scope".into())));
        assert_eq!(parse("getty@tty1.service"), Ok(What::BareUnit("getty@tty1.service".into())));
        for bad in ["", "   ", "user/abc:x.scope", "user/1000", "system:", "cgroup:relative", "a b", "foo:bar"] {
            assert!(parse(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn reply_has_text_and_query() {
        let e = Explanation {
            target: Target::system_unit("x.service"),
            known: false,
            proc: None,
            class: None,
            foreground: false,
            dry_run: true,
            settings: vec![],
            notes: vec![],
        };
        let v: Value = serde_json::from_str(&explanation_json("x.service", &e)).unwrap();
        assert_eq!(v["query"], "x.service");
        assert_eq!(v["known"], false);
        assert_eq!(v["target"]["name"], "x.service");
        assert!(v["text"].as_str().unwrap().contains("not managed by lindesk"));
        let back: Explanation = serde_json::from_value(v).unwrap();
        assert_eq!(back, e, "extra keys don't break deserialising into Explanation");
    }
}
