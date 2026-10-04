//! The `query` tool's statement guard.
//!
//! `lindesk-query` already runs statements in a sandboxed, read-only DuckDB; this is the
//! stricter, cheaper front door: exactly one `SELECT`/`WITH` statement, wrapped so the
//! row cap is enforced by the engine.

/// Hard cap on rows returned by `query`.
pub const MAX_ROWS: u32 = 1000;

/// Default row limit.
pub const DEFAULT_ROWS: u32 = 200;

/// Validate `sql` and return it wrapped with a `LIMIT` of `limit + 1` (one extra row tells
/// the caller the result was truncated).
pub fn guard(sql: &str, limit: u32) -> Result<String, String> {
    if limit == 0 || limit > MAX_ROWS {
        return Err(format!("rejected: limit must be between 1 and {MAX_ROWS} (got {limit})"));
    }
    let body = single_statement(sql)?;
    let first = first_keyword(&body).to_ascii_lowercase();
    if first != "select" && first != "with" {
        return Err(format!(
            "rejected: only read-only SELECT or WITH … SELECT statements are allowed (got {}); the telemetry lake is read-only from MCP",
            if first.is_empty() { "an empty statement".to_string() } else { first.to_uppercase() }
        ));
    }
    Ok(format!("SELECT * FROM (\n{body}\n) AS lindesk_mcp_q LIMIT {}", limit + 1))
}

/// Strip trailing semicolons/whitespace; reject a second statement. Quotes and comments
/// are honoured so `';'` inside a string literal is fine.
fn single_statement(sql: &str) -> Result<String, String> {
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut end_of_first: Option<usize> = None;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'\'' | b'"' => {
                let q = c;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == q {
                        if i + 1 < bytes.len() && bytes[i + 1] == q {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                if i >= bytes.len() {
                    return Err("rejected: unterminated quoted string".into());
                }
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => match sql[i + 2..].find("*/") {
                Some(off) => i = i + 2 + off + 1,
                None => return Err("rejected: unterminated /* comment".into()),
            },
            b';' if end_of_first.is_none() => end_of_first = Some(i),
            b';' => {}
            _ if !c.is_ascii_whitespace() && end_of_first.is_some() => {
                return Err("rejected: exactly one statement is allowed".into());
            }
            _ => {}
        }
        i += 1;
    }
    let body = &sql[..end_of_first.unwrap_or(sql.len())];
    let body = body.trim();
    if body.is_empty() {
        return Err("rejected: empty statement".into());
    }
    Ok(body.to_string())
}

/// The first keyword, skipping whitespace, comments and opening parentheses.
fn first_keyword(sql: &str) -> &str {
    let mut s = sql;
    loop {
        let t = s.trim_start();
        if let Some(rest) = t.strip_prefix("--") {
            s = rest.split_once('\n').map_or("", |(_, r)| r);
        } else if let Some(rest) = t.strip_prefix("/*") {
            s = rest.split_once("*/").map_or("", |(_, r)| r);
        } else if let Some(rest) = t.strip_prefix('(') {
            s = rest;
        } else {
            s = t;
            break;
        }
    }
    let end = s.find(|c: char| !c.is_ascii_alphanumeric() && c != '_').unwrap_or(s.len());
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_select_and_with() {
        let q = guard("SELECT 1;", 10).unwrap();
        assert!(q.ends_with("LIMIT 11"), "{q}");
        assert!(q.contains("SELECT 1\n"), "{q}");
        assert!(guard("  -- note\n with x as (select 1) select * from x", 5).is_ok());
        assert!(guard("(select 1)", 5).is_ok());
        assert!(guard("select ';' as semi; ", 5).is_ok());
        assert!(guard("select 1 /* ; */", 5).is_ok());
    }

    #[test]
    fn rejects_everything_else() {
        for bad in [
            "DELETE FROM events",
            "insert into x values (1)",
            "COPY events TO '/tmp/x.csv'",
            "ATTACH '/tmp/x.db'",
            "PRAGMA show_tables",
            "SET threads = 1",
            "INSTALL httpfs",
            "",
            "   ;",
        ] {
            let e = guard(bad, 10).unwrap_err();
            assert!(e.starts_with("rejected"), "{bad}: {e}");
        }
        assert!(guard("select 1; drop table x", 10).unwrap_err().contains("one statement"));
        assert!(guard("select 'oops", 10).is_err());
        assert!(guard("select 1", 0).is_err());
        assert!(guard("select 1", 1001).unwrap_err().contains("1000"));
    }
}
