//! Comment-preserving edits of lindesk.toml with `toml_edit`.

use serde_json::{Map, Value as Json};
use toml_edit::{ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

/// Parse a config file (empty text is an empty document).
pub fn parse(text: &str) -> Result<DocumentMut, String> {
    text.parse::<DocumentMut>().map_err(|e| e.to_string())
}

/// Split and check a dotted key: segments of `[A-Za-z0-9_-]`.
pub fn split_key(key: &str) -> Result<Vec<String>, String> {
    let parts: Vec<String> = key.split('.').map(str::to_string).collect();
    if key.is_empty()
        || parts.len() > 12
        || parts.iter().any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
    {
        return Err(format!("invalid key '{key}' (dotted path of [A-Za-z0-9_-] segments)"));
    }
    Ok(parts)
}

/// The value at a dotted path as JSON (`null` when absent).
pub fn get(doc: &DocumentMut, path: &[String]) -> Json {
    let mut item: &Item = doc.as_item();
    for seg in path {
        match item.as_table_like().and_then(|t| t.get(seg)) {
            Some(i) => item = i,
            None => return Json::Null,
        }
    }
    item_to_json(item)
}

/// Set a dotted path to a JSON value, creating tables as needed and keeping the existing
/// key's comments and formatting.
pub fn set(doc: &mut DocumentMut, path: &[String], v: &Json) -> Result<(), String> {
    if v.is_null() {
        unset(doc, path);
        return Ok(());
    }
    let (last, parents) = path.split_last().ok_or("empty key")?;
    let mut table: &mut dyn toml_edit::TableLike = doc.as_table_mut();
    let mut inline = false;
    for seg in parents {
        if table.get(seg).is_none() {
            if inline {
                table.insert(seg, Item::Value(Value::InlineTable(InlineTable::new())));
            } else {
                let mut t = Table::new();
                t.set_implicit(true);
                table.insert(seg, Item::Table(t));
            }
        }
        let item = table.get_mut(seg).ok_or("unreachable")?;
        inline = matches!(item, Item::Value(_));
        table = item.as_table_like_mut().ok_or_else(|| format!("'{seg}' is not a table"))?;
    }
    match table.get_mut(last) {
        Some(item @ Item::Value(_)) => {
            let old = item.as_value().cloned();
            let mut nv = json_to_value(v, old.as_ref())?;
            if let Some(old) = old {
                *nv.decor_mut() = old.decor().clone();
            }
            *item = Item::Value(nv);
        }
        Some(item @ Item::Table(_)) if v.is_object() => {
            let decor = item.as_table().map(|t| t.decor().clone()).unwrap_or_default();
            let mut t = json_to_table(v)?;
            *t.decor_mut() = decor;
            *item = Item::Table(t);
        }
        Some(Item::ArrayOfTables(_)) => return Err(format!("'{last}' is an array of tables")),
        Some(item) => *item = Item::Value(json_to_value(v, None)?),
        None if !inline && v.is_object() => {
            table.insert(last, Item::Table(json_to_table(v)?));
        }
        None => {
            table.insert(last, Item::Value(json_to_value(v, None)?));
        }
    }
    Ok(())
}

/// Remove a dotted path. Tables left empty (and carrying no comments) are removed too, so
/// set + unset round-trips to the original text. Returns whether something was removed.
pub fn unset(doc: &mut DocumentMut, path: &[String]) -> bool {
    fn rec(t: &mut dyn toml_edit::TableLike, path: &[String]) -> bool {
        match path {
            [] => false,
            [last] => t.remove(last).is_some(),
            [seg, rest @ ..] => {
                let Some(child) = t.get_mut(seg).and_then(|i| i.as_table_like_mut()) else { return false };
                let removed = rec(child, rest);
                if removed && child.is_empty() {
                    let keep = match t.get(seg) {
                        Some(Item::Table(tab)) => has_comment(tab.decor()),
                        Some(Item::Value(v)) => has_comment(v.decor()),
                        _ => false,
                    };
                    if !keep {
                        t.remove(seg);
                    }
                }
                removed
            }
        }
    }
    rec(doc.as_table_mut(), path)
}

fn has_comment(d: &toml_edit::Decor) -> bool {
    let c = |r: Option<&toml_edit::RawString>| r.and_then(|r| r.as_str()).is_some_and(|s| s.contains('#'));
    c(d.prefix()) || c(d.suffix())
}

/// `[[override]]` entry pinning `unit` (and no `comm`): its index.
fn find_override(aot: &ArrayOfTables, unit: &str) -> Option<usize> {
    aot.iter().position(|t| t.get("unit").and_then(|i| i.as_str()) == Some(unit) && !t.contains_key("comm"))
}

/// The class pinned on `unit` by an `[[override]]` (`null` when there is none).
pub fn get_override_class(doc: &DocumentMut, unit: &str) -> Json {
    doc.get("override")
        .and_then(|i| i.as_array_of_tables())
        .and_then(|aot| find_override(aot, unit).and_then(|i| aot.get(i)))
        .and_then(|t| t.get("class"))
        .map(item_to_json)
        .unwrap_or(Json::Null)
}

/// Pin `class` on `unit` (string), or remove the pin (`null`): an entry left with only
/// `unit` is removed, and the array when it becomes empty.
pub fn set_override_class(doc: &mut DocumentMut, unit: &str, class: &Json) -> Result<(), String> {
    if doc.get("override").is_some_and(|i| i.as_array_of_tables().is_none()) {
        return Err("`override` is not written as [[override]] tables; edit it by hand".into());
    }
    match class {
        Json::String(c) => {
            let aot = doc
                .entry("override")
                .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()))
                .as_array_of_tables_mut()
                .ok_or("override is not an array of tables")?;
            match find_override(aot, unit).and_then(|i| aot.get_mut(i)) {
                Some(t) => match t.get_mut("class") {
                    Some(Item::Value(v)) => {
                        let decor = v.decor().clone();
                        let mut nv = Value::from(c.as_str());
                        *nv.decor_mut() = decor;
                        *v = nv;
                    }
                    _ => {
                        t.insert("class", toml_edit::value(c.as_str()));
                    }
                },
                None => {
                    let mut t = Table::new();
                    t.insert("unit", toml_edit::value(unit));
                    t.insert("class", toml_edit::value(c.as_str()));
                    aot.push(t);
                }
            }
            Ok(())
        }
        Json::Null => {
            let Some(aot) = doc.get_mut("override").and_then(|i| i.as_array_of_tables_mut()) else { return Ok(()) };
            if let Some(i) = find_override(aot, unit) {
                let t = aot.get_mut(i).ok_or("unreachable")?;
                t.remove("class");
                if t.iter().all(|(k, _)| k == "unit") {
                    aot.remove(i);
                }
            }
            if aot.is_empty() {
                doc.remove("override");
            }
            Ok(())
        }
        other => Err(format!("class must be a string, not {other}")),
    }
}

/// JSON → TOML value. `like` is the value being replaced: an integer replacing a float
/// stays a float (so `50` doesn't turn `uclamp_min_max = 50.0` into an integer).
pub fn json_to_value(v: &Json, like: Option<&Value>) -> Result<Value, String> {
    Ok(match v {
        Json::Null => return Err("null is not a TOML value (use config_unset)".into()),
        Json::Bool(b) => Value::from(*b),
        Json::Number(n) => {
            if let (Some(Value::Float(_)), Some(f)) = (like, n.as_f64()) {
                Value::from(f)
            } else if let Some(i) = n.as_i64() {
                Value::from(i)
            } else if let Some(f) = n.as_f64() {
                if n.is_u64() {
                    return Err(format!("{n} is too large for TOML"));
                }
                Value::from(f)
            } else {
                return Err(format!("unsupported number {n}"));
            }
        }
        Json::String(s) => Value::from(s.as_str()),
        Json::Array(a) => {
            let mut arr = toml_edit::Array::new();
            for x in a {
                arr.push(json_to_value(x, None)?);
            }
            Value::Array(arr)
        }
        Json::Object(o) => {
            let mut t = InlineTable::new();
            for (k, x) in o {
                if x.is_null() {
                    continue;
                }
                t.insert(k, json_to_value(x, None)?);
            }
            Value::InlineTable(t)
        }
    })
}

fn json_to_table(v: &Json) -> Result<Table, String> {
    let mut t = Table::new();
    if let Json::Object(o) = v {
        for (k, x) in o {
            match x {
                Json::Null => {}
                Json::Object(_) => {
                    t.insert(k, Item::Table(json_to_table(x)?));
                }
                _ => {
                    t.insert(k, Item::Value(json_to_value(x, None)?));
                }
            }
        }
    }
    Ok(t)
}

pub fn item_to_json(item: &Item) -> Json {
    match item {
        Item::None => Json::Null,
        Item::Value(v) => value_to_json(v),
        Item::Table(t) => Json::Object(t.iter().map(|(k, i)| (k.to_string(), item_to_json(i))).collect::<Map<_, _>>()),
        Item::ArrayOfTables(a) => Json::Array(
            a.iter().map(|t| Json::Object(t.iter().map(|(k, i)| (k.to_string(), item_to_json(i))).collect())).collect(),
        ),
    }
}

pub fn value_to_json(v: &Value) -> Json {
    match v {
        Value::String(s) => Json::String(s.value().clone()),
        Value::Integer(i) => Json::from(*i.value()),
        Value::Float(f) => serde_json::Number::from_f64(*f.value()).map(Json::Number).unwrap_or(Json::Null),
        Value::Boolean(b) => Json::Bool(*b.value()),
        Value::Datetime(d) => Json::String(d.value().to_string()),
        Value::Array(a) => Json::Array(a.iter().map(value_to_json).collect()),
        Value::InlineTable(t) => Json::Object(t.iter().map(|(k, v)| (k.to_string(), value_to_json(v))).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SAMPLE: &str = r#"# lindesk configuration
[general]
# Pipeline tick.
tick = "250ms"   # trailing comment
dry_run = false

[plugins.threshold]
enabled = true

[plugins.threshold.config.values]
# boost
fg_epoch_uclamp_min = 30.0
"#;

    fn k(s: &str) -> Vec<String> {
        split_key(s).unwrap()
    }

    #[test]
    fn edits_preserve_comments_and_round_trip() {
        let mut doc = parse(SAMPLE).unwrap();
        set(&mut doc, &k("general.tick"), &json!("100ms")).unwrap();
        let out = doc.to_string();
        assert!(out.contains("# Pipeline tick.\ntick = \"100ms\"   # trailing comment\n"), "{out}");
        assert!(out.starts_with("# lindesk configuration\n"));
        set(&mut doc, &k("general.tick"), &json!("250ms")).unwrap();
        assert_eq!(doc.to_string(), SAMPLE);

        // New key in an existing table, then removed again.
        set(&mut doc, &k("general.profile"), &json!("gaming")).unwrap();
        assert!(doc.to_string().contains("dry_run = false\nprofile = \"gaming\"\n"));
        assert_eq!(get(&doc, &k("general.profile")), json!("gaming"));
        unset(&mut doc, &k("general.profile"));
        assert_eq!(doc.to_string(), SAMPLE);

        // New nested tables are pruned on unset.
        set(&mut doc, &k("telemetry.retention.raw"), &json!("3d")).unwrap();
        assert_eq!(get(&doc, &k("telemetry.retention")), json!({"raw": "3d"}));
        unset(&mut doc, &k("telemetry.retention.raw"));
        assert_eq!(doc.to_string(), SAMPLE);

        // Float stays float.
        set(&mut doc, &k("plugins.threshold.config.values.fg_epoch_uclamp_min"), &json!(40)).unwrap();
        assert!(doc.to_string().contains("# boost\nfg_epoch_uclamp_min = 40.0\n"));
    }

    #[test]
    fn inline_tables_and_overrides() {
        let mut doc = parse(SAMPLE).unwrap();
        // `[plugins.threshold.config.values]` and `config = {...}` can't both exist in valid
        // TOML, so use a document with only the inline form.
        let mut doc2 = parse("[plugins.threshold]\nconfig = { epoch = \"300ms\" }\n").unwrap();
        set(&mut doc2, &k("plugins.threshold.config.values.fg_weight"), &json!(300)).unwrap();
        assert_eq!(get(&doc2, &k("plugins.threshold.config")), json!({"epoch": "300ms", "values": {"fg_weight": 300}}));
        unset(&mut doc2, &k("plugins.threshold.config.values.fg_weight"));
        assert_eq!(get(&doc2, &k("plugins.threshold.config")), json!({"epoch": "300ms"}));

        assert_eq!(get_override_class(&doc, "steam*.scope"), Json::Null);
        set_override_class(&mut doc, "steam*.scope", &json!("interactive")).unwrap();
        assert!(doc.to_string().ends_with("[[override]]\nunit = \"steam*.scope\"\nclass = \"interactive\"\n"));
        assert_eq!(get_override_class(&doc, "steam*.scope"), json!("interactive"));
        set_override_class(&mut doc, "steam*.scope", &json!("batch")).unwrap();
        assert_eq!(get_override_class(&doc, "steam*.scope"), json!("batch"));
        set_override_class(&mut doc, "steam*.scope", &Json::Null).unwrap();
        assert_eq!(doc.to_string(), SAMPLE);
        assert!(split_key("a..b").is_err() && split_key("a.b c").is_err() && split_key("").is_err());
    }
}
