//! End-to-end tests of the telemetry lake: lindesk-store writes, lindesk-query reads and
//! maintains.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use common::*;
use lindesk_core::config::Retention;
use lindesk_plugin_api::*;
use lindesk_query::{Backend, BackendChoice, DeleteRequest, Format, Lake, QueryError};
use lindesk_store::{BenchPhase, BenchRun, BenchSample};

fn write_then_query_round_trip(choice: BackendChoice) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("telemetry");
    let mut s = store(&root);
    let records = mix(BASE, 1, 10);
    s.append(&records);
    s.append_bench(
        &BenchRun {
            run_id: "r1".into(),
            ts: BASE,
            label: "torture".into(),
            host: Some("box".into()),
            kernel: None,
            config: serde_json::json!({"load": "fio"}),
            notes: None,
        },
        &[BenchSample {
            ts: BASE + 1,
            run_id: "r1".into(),
            phase: BenchPhase::LindeskOn,
            metric: "wakeup_p99_us".into(),
            value: 120.5,
            iteration: Some(1),
            labels: Default::default(),
        }],
    );
    let r = s.flush(BASE + 2 * HOUR).unwrap();
    assert_eq!(r.rows, records.len() + 2);
    assert_eq!(r.deferred_rows, 0);

    let lake = lake(&root, choice);
    let expect = |table: &str| records.iter().filter(|r| r.table() == table).count() as i64;
    for t in ["pressure", "units", "events", "metrics"] {
        assert_eq!(count(&lake, t), expect(t), "{t}");
    }
    assert_eq!(count(&lake, "bench_runs"), 1);
    assert_eq!(count(&lake, "bench_samples"), 1);
    // Tables without files are empty, typed views rather than errors.
    for t in ["pressure_1m", "pressure_1h", "units_1m", "units_1h"] {
        assert_eq!(count(&lake, t), 0, "{t}");
    }
    assert_eq!(scalar(&lake, "SELECT count(*) FROM pressure_1m WHERE some_avg10_p95 > 1"), 0);

    // Values, types and hive columns survive the round trip.
    assert_eq!(scalar(&lake, "SELECT epoch_ms(min(ts)) FROM pressure"), BASE);
    assert_eq!(scalar(&lake, "SELECT count(DISTINCT hour) FROM pressure WHERE date = DATE '2026-01-01'"), 1);
    assert_eq!(
        scalar(&lake, "SELECT count(*) FROM pressure WHERE resource = 'cpu' AND full_avg10 IS NULL"),
        expect("pressure") / 3
    );
    assert_eq!(scalar(&lake, "SELECT count(*) FROM metrics WHERE labels->>'host' = 'laptop'"), expect("metrics"));
    assert_eq!(
        scalar(&lake, "SELECT count(*) FROM events WHERE payload->>'$.kind' = 'focus' AND uid = 1000 AND pid = 4242"),
        expect("events")
    );
    assert_eq!(
        scalar(&lake, "SELECT count(*) FROM units WHERE manager = 'user' AND (applied->>'$[0].knob') = 'cpu_weight'"),
        expect("units")
    );
    let v = lake.query("SELECT config->>'$.load' AS load, host FROM bench_runs", &Format::Json).unwrap();
    let v: serde_json::Value = serde_json::from_str(&v).unwrap();
    assert_eq!(v, serde_json::json!([{"load": "fio", "host": "box"}]));

    // Table output is aligned.
    let t = lake.query("SELECT resource, count(*) AS n FROM pressure GROUP BY 1 ORDER BY 1", &Format::Table).unwrap();
    let lines: Vec<&str> = t.lines().collect();
    assert!(lines[0].starts_with("+-"), "{t}");
    assert!(lines[1].contains("resource") && lines[1].contains("| n "), "{t}");
    assert_eq!(
        lines.iter().filter(|l| l.starts_with('|')).map(|l| l.len()).collect::<std::collections::BTreeSet<_>>().len(),
        1
    );
    assert!(t.ends_with("(3 rows)\n"), "{t}");

    // Inspection statements work.
    assert!(lake.query("DESCRIBE pressure", &Format::Csv).unwrap().contains("some_avg10"));
    assert!(lake.query("SHOW TABLES", &Format::Csv).unwrap().contains("units_1h"));
    assert!(lake.query("PRAGMA show_tables", &Format::Csv).unwrap().contains("bench_samples"));

    // Parquet export.
    let out = dir.path().join("export.parquet");
    lake.query("SELECT * FROM units WHERE foreground", &Format::Parquet(out.clone())).unwrap();
    assert!(out.exists());
}

fn query_is_read_only_and_sandboxed(choice: BackendChoice) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("telemetry");
    let mut s = store(&root);
    s.append(&mix(BASE, 1, 60));
    s.flush(BASE).unwrap();
    let lake = lake(&root, choice);
    std::fs::write(dir.path().join("secret.csv"), "a\n1\n").unwrap();
    let secret = dir.path().join("secret.csv").display().to_string();
    let refused = |sql: &str| match lake.query(sql, &Format::Csv) {
        Err(_) => {}
        Ok(out) => panic!("{sql} should fail, got {out}"),
    };
    refused("CREATE TABLE t AS SELECT 1");
    refused("COPY pressure TO '/tmp/x.csv'");
    refused("ATTACH '/tmp/x.duckdb'");
    refused("INSTALL httpfs");
    refused("SET enable_external_access = true");
    refused("SELECT 1; DROP VIEW pressure");
    refused("SELECT 1; COPY pressure TO '/tmp/x.csv'");
    refused(&format!("SELECT * FROM read_csv('{secret}')"));
    refused("SELECT * FROM read_text('/etc/hostname')");
    assert!(matches!(lake.query("DELETE FROM pressure", &Format::Csv), Err(QueryError::Refused(_))));
    // Reading the lake's own files directly is fine (with the extension).
    if lake.backend().unwrap() == Backend::Extension {
        let own = format!("SELECT count(*) FROM read_vortex('{}/pressure/**/*.vortex')", lake.root().display());
        assert!(scalar(&lake, &own) > 0);
    }
}

fn compaction_merges_sealed_partitions_and_keeps_rows(choice: BackendChoice) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("telemetry");
    let mut s = store(&root);
    // Three flushes over hours 0..3; hour 2 is "current".
    for chunk in 0..3 {
        let recs: Vec<Record> = mix(BASE, 3, 20).into_iter().skip(chunk).step_by(3).collect();
        s.append(&recs);
        s.flush(BASE).unwrap();
    }
    let lake = lake(&root, choice);
    let before: Vec<i64> = ["pressure", "units", "events", "metrics"].iter().map(|t| count(&lake, t)).collect();
    let files_before = data_files(&root.join("pressure")).len();
    assert_eq!(files_before, 9);
    let now = BASE + 2 * HOUR + 10 * 60_000;
    // Every sealed (hour 0 or 1) partition holding more than one file gets merged.
    let mut expect_parts = 0;
    let mut expect_files = 0;
    for t in ["pressure", "units", "events", "metrics"] {
        for h in ["00", "01"] {
            let n = data_files(&root.join(format!("{t}/date=2026-01-01/hour={h}"))).len();
            if n > 1 {
                expect_parts += 1;
                expect_files += n;
            }
        }
    }
    assert!(expect_parts >= 4);
    let r = lake.compact(now).unwrap();
    assert_eq!(r.partitions, expect_parts);
    assert_eq!(r.files_before, expect_files);
    assert_eq!(r.files_after, expect_parts);
    assert!(r.rollup_hours > 0);
    let after: Vec<i64> = ["pressure", "units", "events", "metrics"].iter().map(|t| count(&lake, t)).collect();
    assert_eq!(before, after);
    // Sealed hours have one file each; the current hour is untouched.
    assert_eq!(data_files(&root.join("pressure/date=2026-01-01/hour=00")).len(), 1);
    assert_eq!(data_files(&root.join("pressure/date=2026-01-01/hour=01")).len(), 1);
    assert_eq!(data_files(&root.join("pressure/date=2026-01-01/hour=02")).len(), 3);
    // Compacted files are sorted by the table's sort key.
    let merged = &data_files(&root.join("pressure/date=2026-01-01/hour=01"))[0];
    assert!(sorted_by_scope_resource_ts(merged));
    let raw = &data_files(&root.join("pressure/date=2026-01-01/hour=02"))[0];
    assert!(!sorted_by_scope_resource_ts(raw), "raw files are in time order, not sort-key order");
    // Staging is cleaned up and permissions follow the lake (0700 / 0600).
    assert!(std::fs::read_dir(root.join(".staging")).map(|d| d.count()).unwrap_or(0) == 0);
    for f in data_files(&root) {
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600, "{}", f.display());
    }
    assert_eq!(std::fs::metadata(&root).unwrap().permissions().mode() & 0o777, 0o700);

    // Idempotent: nothing to do the second time.
    let again = lake.compact(now).unwrap();
    assert_eq!((again.partitions, again.rollup_hours, again.rollup_partitions), (0, 0, 0));

    // The non-atomic fallback swap gives the same result.
    s.append(&mix(BASE, 1, 30));
    s.flush(BASE).unwrap();
    let total = count(&lake, "pressure");
    let r = lake.clone().with_exchange(false).compact(now).unwrap();
    assert!(r.partitions >= 4);
    assert_eq!(count(&lake, "pressure"), total);
    assert_eq!(data_files(&root.join("pressure/date=2026-01-01/hour=00")).len(), 1);
}

/// Raw aggregates per (group, bucket), compared with the rollup table; returns
/// (matched groups, mismatching groups, raw groups).
fn rollup_mismatches(lake: &Lake, rollup: &str, bucket_us: i64, now: i64) -> (i64, i64, i64) {
    let sealed = format!("ts < make_timestamp({}::BIGINT)::TIMESTAMPTZ", (now / HOUR * HOUR) * 1000);
    let (raw, keys, checks) = if rollup.starts_with("pressure") {
        (
            format!(
                "SELECT scope, resource, epoch_us(ts) // {bucket_us} AS b, count(*) AS n, sum(some_delta_us) AS d, \
                 sum(full_delta_us) AS fd, min(some_avg10) AS mn, max(some_avg10) AS mx, avg(some_avg10) AS a, \
                 quantile_cont(some_avg10, 0.95) AS p, max(full_avg10) AS fmx FROM pressure WHERE {sealed} GROUP BY ALL"
            ),
            "u.scope = r.scope AND u.resource = r.resource",
            "u.samples != r.n OR u.some_delta_us != r.d OR u.full_delta_us IS DISTINCT FROM r.fd \
             OR u.some_avg10_min != r.mn OR u.some_avg10_max != r.mx OR abs(u.some_avg10_avg - r.a) > 1e-9 \
             OR abs(u.some_avg10_p95 - r.p) > 1e-9 OR u.full_avg10_max IS DISTINCT FROM r.fmx",
        )
    } else {
        (
            format!(
                "SELECT unit, epoch_us(ts) // {bucket_us} AS b, count(*) AS n, sum(cpu_usage_us) AS c, \
                 sum(io_wbytes) AS w, max(mem_current) AS mx, avg(mem_current) AS a, bool_or(foreground) AS fg, \
                 arg_max(class, ts) AS cls FROM units WHERE {sealed} GROUP BY ALL"
            ),
            "u.unit = r.unit",
            "u.samples != r.n OR u.cpu_usage_us != r.c OR u.io_wbytes != r.w OR u.mem_current_max != r.mx \
             OR abs(u.mem_current_avg - r.a) > 1e-6 OR u.foreground != r.fg OR u.class != r.cls",
        )
    };
    let sql = format!(
        "WITH r AS ({raw}) SELECT count(u.ts) AS matched, count(*) FILTER (WHERE u.ts IS NULL OR {checks}) AS bad, \
         count(*) AS total FROM r LEFT JOIN {rollup} u ON {keys} AND epoch_us(u.ts) // {bucket_us} = r.b"
    );
    let out = lake.query(&sql, &Format::Csv).unwrap();
    let vals: Vec<i64> = out.lines().nth(1).unwrap().split(',').map(|v| v.parse().unwrap()).collect();
    (vals[0], vals[1], vals[2])
}

fn rollups_match_raw_aggregates_and_are_idempotent(choice: BackendChoice) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("telemetry");
    let mut s = store(&root);
    s.append(&mix(BASE + 22 * HOUR, 4, 15)); // crosses a day boundary
    s.flush(BASE).unwrap();
    let lake = lake(&root, choice);
    let now = BASE + 25 * HOUR + 30 * 60_000;
    lake.compact(now).unwrap();
    let check = |lake: &Lake| {
        for (t, us) in [
            ("pressure_1m", 60_000_000),
            ("pressure_1h", 3_600_000_000),
            ("units_1m", 60_000_000),
            ("units_1h", 3_600_000_000),
        ] {
            let (matched, bad, total) = rollup_mismatches(lake, t, us, now);
            assert!(total > 0, "{t}");
            assert_eq!((matched, bad), (total, 0), "{t}");
            // No rollup rows beyond the raw groups (no double counting).
            assert_eq!(count(lake, t), total, "{t}");
        }
    };
    check(&lake);
    assert_eq!(data_files(&root.join("pressure_1h")).len(), 2, "one file per day");
    assert_eq!(count(&lake, "pressure_1h"), 3 * 3, "3 sealed hours x 3 resources");

    // A late file lands in a sealed hour: the next compaction merges it and recomputes
    // exactly that hour's rollups.
    s.append(&[pressure(BASE + 23 * HOUR + 1, "system", Resource::Io, 99.0, 777)]);
    s.flush(BASE).unwrap();
    let r = lake.compact(now).unwrap();
    assert_eq!(r.partitions, 1);
    assert_eq!(r.rollup_hours, 2, "pressure_1m and pressure_1h, one hour each");
    check(&lake);
    assert_eq!(
        scalar(&lake, "SELECT sum(samples) FROM pressure_1h"),
        scalar(
            &lake,
            &format!(
                "SELECT count(*) FROM pressure WHERE ts < make_timestamp({}::BIGINT)::TIMESTAMPTZ",
                (now / HOUR * HOUR) * 1000
            )
        )
    );

    // Once the last hour seals, it is rolled up too.
    let later = BASE + 27 * HOUR;
    lake.compact(later).unwrap();
    assert_eq!(count(&lake, "pressure_1h"), 4 * 3);
}

fn prune_by_ttl_and_size(choice: BackendChoice) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("telemetry");
    let mut s = store(&root);
    for day in 0..6 {
        s.append(&mix(BASE + day * DAY, 2, 60));
    }
    s.append_bench(
        &BenchRun {
            run_id: "old".into(),
            ts: BASE,
            label: "x".into(),
            host: None,
            kernel: None,
            config: serde_json::Value::Null,
            notes: None,
        },
        &[],
    );
    s.flush(BASE).unwrap();
    let lake = lake(&root, choice);
    let now = BASE + 6 * DAY;
    lake.compact(now).unwrap();
    assert_eq!(scalar(&lake, "SELECT count(DISTINCT date) FROM pressure"), 6);

    let retention = Retention {
        raw: Duration::from_secs(3 * 86_400),
        rollup_1m: Duration::from_secs(4 * 86_400),
        rollup_1h: Duration::from_secs(30 * 86_400),
        max_size: 0,
    };
    let r = lake.prune(&retention, now).unwrap();
    assert!(r.bytes_after < r.bytes_before);
    // Raw keeps days 3..5, 1m keeps days 2..5, 1h keeps everything; bench is never pruned.
    assert_eq!(scalar(&lake, "SELECT count(DISTINCT date) FROM pressure"), 3);
    assert_eq!(scalar(&lake, "SELECT epoch_ms(min(ts)) FROM pressure"), BASE + 3 * DAY);
    assert_eq!(scalar(&lake, "SELECT count(DISTINCT date) FROM pressure_1m"), 4);
    assert_eq!(scalar(&lake, "SELECT count(DISTINCT date) FROM units_1h"), 6);
    assert_eq!(count(&lake, "bench_runs"), 1);
    let expired: usize = r.tables.iter().map(|t| t.expired).sum();
    assert_eq!(expired, 4 * 3 * 2 + 2 * 2, "4 raw tables x 3 days x 2 hours + 2 1m tables x 2 days");
    assert!(!root.join("pressure/date=2026-01-01").exists(), "emptied date directories are removed");

    // Size cap: the oldest raw partitions go first, rollups survive.
    let size = lake.stats().unwrap().bytes;
    let cap = Retention { max_size: size * 3 / 4, ..retention };
    let r = lake.prune(&cap, now).unwrap();
    assert!(r.bytes_after <= cap.max_size, "{r:?}");
    assert!(r.tables.iter().any(|t| t.evicted > 0));
    assert!(scalar(&lake, "SELECT epoch_ms(min(ts)) FROM pressure") > BASE + 3 * DAY);
    assert_eq!(scalar(&lake, "SELECT epoch_ms(max(ts)) FROM pressure") / HOUR, (BASE + 5 * DAY + HOUR) / HOUR);
    assert_eq!(scalar(&lake, "SELECT count(DISTINCT date) FROM units_1h"), 6);
    assert_eq!(lake.stats().unwrap().bytes, r.bytes_after);
}

fn delete_rewrites_raw_and_rollups_only_where_rows_match(choice: BackendChoice) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("telemetry");
    let mut s = store(&root);
    s.append(&mix(BASE, 3, 30));
    s.flush(BASE).unwrap();
    let lake = lake(&root, choice);
    let now = BASE + 3 * HOUR;
    lake.compact(now).unwrap();

    let ff = |t: &str| scalar(&lake, &format!("SELECT count(*) FROM {t} WHERE unit = 'app-firefox.scope'"));
    let other =
        |t: &str| scalar(&lake, &format!("SELECT count(*) FROM {t} WHERE unit IS DISTINCT FROM 'app-firefox.scope'"));
    let tables = ["units", "events", "units_1m", "units_1h"];
    let before: Vec<(i64, i64)> = tables.iter().map(|t| (ff(t), other(t))).collect();
    assert!(before.iter().all(|(f, o)| *f > 0 && *o > 0), "{before:?}");
    let pressure = count(&lake, "pressure");
    let metrics = count(&lake, "metrics");

    // Bad predicates are rejected before anything changes.
    for bad in ["no_such_column = 1", "unit = ", "1=1; select 1", "unit = 'x') OR (1=1"] {
        assert!(lake.delete(&DeleteRequest { predicate: bad.into(), ..Default::default() }).is_err(), "{bad}");
    }

    // Dry run only counts.
    let req = DeleteRequest { predicate: "unit = 'app-firefox.scope'".into(), dry_run: true, ..Default::default() };
    let dry = lake.delete(&req).unwrap();
    assert_eq!(dry.rows() as i64, before.iter().map(|(f, _)| f).sum::<i64>());
    assert_eq!(ff("units"), before[0].0);

    // Time-bounded delete only touches hour 0.
    let r = lake.delete(&DeleteRequest { until: Some(BASE + HOUR), dry_run: false, ..req.clone() }).unwrap();
    assert!(r.tables.iter().all(|t| t.partitions_rewritten + t.partitions_removed == 1), "{r:?}");
    assert_eq!(scalar(&lake, "SELECT count(*) FROM units WHERE unit = 'app-firefox.scope' AND hour = 0"), 0);
    assert!(scalar(&lake, "SELECT count(*) FROM units WHERE unit = 'app-firefox.scope' AND hour = 1") > 0);

    let r = lake.delete(&DeleteRequest { dry_run: false, ..req }).unwrap();
    assert!(r.skipped.iter().any(|(t, _)| t == "pressure"), "{r:?}");
    for (t, (_, o)) in tables.iter().zip(&before) {
        assert_eq!(ff(t), 0, "{t}");
        assert_eq!(other(t), *o, "{t} must keep other rows");
    }
    assert_eq!(count(&lake, "pressure"), pressure);
    assert_eq!(count(&lake, "metrics"), metrics);

    // Rollups recomputed later stay consistent with the filtered raw data.
    lake.compact(now + HOUR).unwrap();
    assert_eq!(ff("units_1h"), 0);
    assert_eq!(
        scalar(&lake, "SELECT sum(samples) FROM units_1h"),
        scalar(
            &lake,
            &format!(
                "SELECT count(*) FROM units WHERE ts < make_timestamp({}::BIGINT)::TIMESTAMPTZ",
                (now + HOUR) * 1000
            )
        )
    );

    // An app_id delete only applies to events.
    let r = lake
        .delete(&DeleteRequest { predicate: "app_id = 'org.gnome.Terminal'".into(), ..Default::default() })
        .unwrap();
    assert_eq!(r.tables.iter().map(|t| t.table.as_str()).collect::<Vec<_>>(), ["events"]);
    assert_eq!(scalar(&lake, "SELECT count(*) FROM events WHERE app_id = 'org.gnome.Terminal'"), 0);
    // A whole-partition match removes the partition.
    let r = lake
        .delete(&DeleteRequest { predicate: "true".into(), tables: vec!["metrics".into()], ..Default::default() })
        .unwrap();
    assert_eq!(r.tables[0].partitions_removed, 3);
    assert_eq!(count(&lake, "metrics"), 0);
}

/// Opens the catalog the way an external DuckDB client would: read-only, then
/// `LOAD vortex` (the documented setup step).
fn open_catalog(cat: &std::path::Path) -> duckdb::Connection {
    let config = duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly).unwrap();
    let conn = duckdb::Connection::open_with_flags(cat, config).unwrap();
    conn.execute_batch(&format!("SET extension_directory = '{}'; LOAD vortex;", ext_dir().display())).unwrap();
    conn
}

/// True when the rows of a Vortex `pressure` file are ordered by (scope, resource, ts).
fn sorted_by_scope_resource_ts(path: &std::path::Path) -> bool {
    use lindesk_store::arrow::arrow_array::{Array, StringArray, TimestampMicrosecondArray};
    let def = lindesk_store::table("pressure").unwrap();
    let batches = lindesk_store::format::VortexCodec::new().read(path, def.arrow_schema()).unwrap();
    let mut keys = Vec::new();
    for b in &batches {
        let col = |n: &str| b.column_by_name(n).unwrap().clone();
        let scope = col("scope");
        let scope = scope.as_any().downcast_ref::<StringArray>().unwrap();
        let res = col("resource");
        let res = res.as_any().downcast_ref::<StringArray>().unwrap();
        let ts = col("ts");
        let ts = ts.as_any().downcast_ref::<TimestampMicrosecondArray>().unwrap();
        for i in 0..b.num_rows() {
            keys.push((scope.value(i).to_string(), res.value(i).to_string(), ts.value(i)));
        }
    }
    keys.windows(2).all(|w| w[0] <= w[1])
}

fn catalog_opens_read_only_in_duckdb(choice: BackendChoice) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("telemetry");
    let mut s = store(&root);
    s.append(&mix(BASE, 2, 60));
    s.flush(BASE).unwrap();
    let lake = lake(&root, choice);
    lake.compact(BASE + 2 * HOUR).unwrap();
    if lake.backend().unwrap() == Backend::Native {
        // Views over read_vortex need the extension; without it the catalog is refused.
        let err = lake.catalog().unwrap_err().to_string();
        assert!(err.contains("vortex extension"), "{err}");
        return;
    }
    let cat = lake.catalog().unwrap();
    assert_eq!(cat, dir.path().join("catalog.duckdb"));
    assert_eq!(std::fs::metadata(&cat).unwrap().permissions().mode() & 0o777, 0o600);

    let conn = open_catalog(&cat);
    let n = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert_eq!(n("SELECT count(*) FROM pressure"), count(&lake, "pressure"));
    assert_eq!(n("SELECT count(*) FROM pressure_1h"), 2 * 3);
    assert_eq!(n("SELECT count(*) FROM bench_runs"), 0, "empty tables are typed empty views");
    assert_eq!(n("SELECT count(*) FROM duckdb_tables()"), 0, "only views");
    assert_eq!(n("SELECT count(*) FROM duckdb_views() WHERE NOT internal"), lindesk_store::TABLES.len() as i64 + 2);
    assert_eq!(n("SELECT count(*) FROM lindesk_tables"), lindesk_store::TABLES.len() as i64);
    assert!(conn.execute_batch("CREATE TABLE x (a INT)").is_err(), "catalog is read-only");
    drop(conn);

    // Regenerating replaces the file atomically and picks up new tables.
    s.append_bench(
        &BenchRun {
            run_id: "r".into(),
            ts: BASE,
            label: "l".into(),
            host: None,
            kernel: None,
            config: serde_json::Value::Null,
            notes: None,
        },
        &[],
    );
    s.flush(BASE).unwrap();
    lake.catalog().unwrap();
    let conn = open_catalog(&cat);
    assert_eq!(conn.query_row("SELECT count(*) FROM bench_runs", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty());
}

fn purge_and_stats(choice: BackendChoice) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("telemetry");
    let mut s = store(&root);
    let recs = mix(BASE, 2, 60);
    s.append(&recs);
    s.flush(BASE).unwrap();
    let lake = lake(&root, choice);
    let st = lake.stats().unwrap();
    let p = st.tables.iter().find(|t| t.table == "pressure").unwrap();
    assert_eq!(p.partitions, 2);
    assert_eq!(p.files, 2);
    assert_eq!(p.rows as usize, recs.iter().filter(|r| r.table() == "pressure").count());
    assert_eq!(p.oldest.as_deref(), Some("2026-01-01 00:00"));
    assert_eq!(p.newest.as_deref(), Some("2026-01-01 01:00"));
    assert_eq!(st.rows as usize, recs.len());

    let r = lake.purge().unwrap();
    assert_eq!(r.files, 8);
    assert_eq!(lake.stats().unwrap().rows, 0);
    assert_eq!(count(&lake, "pressure"), 0);
    // With the extension the catalog is regenerated (empty); without it, removed.
    assert_eq!(lake.catalog_path().exists(), lake.backend().unwrap() == Backend::Extension);
    // The writer keeps working after a purge.
    s.append(&recs[..3]);
    s.flush(BASE).unwrap();
    assert_eq!(count(&lake, "pressure"), 3);
}

fn interrupted_operations_are_recovered(choice: BackendChoice) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("telemetry");
    let mut s = store(&root);
    s.append(&mix(BASE, 2, 60));
    s.flush(BASE).unwrap();
    let lake = lake(&root, choice);
    let n = count(&lake, "pressure");
    let live0 = root.join("pressure/date=2026-01-01/hour=00");
    let live1 = root.join("pressure/date=2026-01-01/hour=01");
    // Crash after staging a copy of a live partition: the staged copy is discarded.
    let staged0 = root.join(".staging/01CRASHED/pressure/date=2026-01-01/hour=00");
    std::fs::create_dir_all(&staged0).unwrap();
    for f in data_files(&live0) {
        std::fs::copy(&f, staged0.join(format!("part-copy-{}", f.file_name().unwrap().to_string_lossy()))).unwrap();
    }
    // Crash in the middle of a fallback swap: the live partition is gone, so it is restored.
    let staged1 = root.join(".staging/01CRASHED/pressure/date=2026-01-01/hour=01");
    std::fs::create_dir_all(staged1.parent().unwrap()).unwrap();
    std::fs::rename(&live1, &staged1).unwrap();
    std::fs::write(staged1.join(".complete"), b"").unwrap();
    // A crash while staging a brand-new partition (no marker): never restored.
    let partial = root.join(".staging/01CRASHED/units/date=2026-01-02/hour=05");
    std::fs::create_dir_all(&partial).unwrap();
    std::fs::write(partial.join("part-x.vortex"), b"truncated").unwrap();
    // A writer crashed mid-file.
    std::fs::write(live0.join(".part-x.vortex.tmp"), b"junk").unwrap();

    let r = lake.compact(BASE + 3 * HOUR).unwrap();
    assert_eq!(r.recovered, 1);
    assert_eq!(count(&lake, "pressure"), n);
    assert!(!root.join(".staging/01CRASHED").exists());
    assert!(!root.join("units/date=2026-01-02").exists());
    assert!(!live1.join(".complete").exists());
    assert!(!live0.join(".part-x.vortex.tmp").exists());
}

both_backends!(write_then_query_round_trip_on, write_then_query_round_trip);
both_backends!(query_is_read_only_and_sandboxed_on, query_is_read_only_and_sandboxed);
both_backends!(
    compaction_merges_sealed_partitions_and_keeps_rows_on,
    compaction_merges_sealed_partitions_and_keeps_rows
);
both_backends!(rollups_match_raw_aggregates_and_are_idempotent_on, rollups_match_raw_aggregates_and_are_idempotent);
both_backends!(prune_by_ttl_and_size_on, prune_by_ttl_and_size);
both_backends!(
    delete_rewrites_raw_and_rollups_only_where_rows_match_on,
    delete_rewrites_raw_and_rollups_only_where_rows_match
);
both_backends!(catalog_opens_read_only_in_duckdb_on, catalog_opens_read_only_in_duckdb);
both_backends!(purge_and_stats_on, purge_and_stats);
both_backends!(interrupted_operations_are_recovered_on, interrupted_operations_are_recovered);
