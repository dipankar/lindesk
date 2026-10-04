//! Lakes written before the switch to Vortex hold `.parquet` files (labels as a MAP,
//! millisecond timestamps). They must stay queryable, and compaction converts them.

mod common;

use common::*;
use lindesk_query::BackendChoice;
use lindesk_store::layout::PartitionKey;

/// Writes a Parquet file shaped like the old writer's output into a partition.
fn legacy_file(root: &std::path::Path, table: &str, ts_ms: i64, select: &str) {
    let g = lindesk_store::table(table).unwrap().granularity();
    let dir = root.join(table).join(PartitionKey::for_ts(ts_ms, g).rel_path());
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("part-01LEGACY00000000000000000.parquet");
    let conn = duckdb::Connection::open_in_memory().unwrap();
    conn.execute_batch(&format!("COPY ({select}) TO '{}' (FORMAT parquet, COMPRESSION zstd)", path.display())).unwrap();
}

fn legacy_lake_keeps_working_and_compaction_converts_it(choice: BackendChoice) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("telemetry");
    std::fs::create_dir_all(&root).unwrap();
    // Old-format rows: one metric and one bench run in hour 0.
    legacy_file(
        &root,
        "metrics",
        BASE,
        "SELECT 1::INTEGER AS schema_version, make_timestamp(1767225601000000::BIGINT)::TIMESTAMPTZ AS ts, \
         'gnome' AS plugin, 'frame_overruns' AS name, 3.0::DOUBLE AS value, MAP {'host': 'laptop'} AS labels",
    );
    legacy_file(
        &root,
        "bench_runs",
        BASE,
        "SELECT 1::INTEGER AS schema_version, make_timestamp(1767225600000000::BIGINT)::TIMESTAMPTZ AS ts, \
         'old-run' AS run_id, 'baseline' AS label, 'box' AS host, NULL::VARCHAR AS kernel, '{}' AS config, \
         NULL::VARCHAR AS notes",
    );
    // New rows next to them, in the same and the next hour.
    let mut s = store(&root);
    s.append(&[
        metric(BASE + 5_000, "frame_overruns", 1.0, "laptop"),
        metric(BASE + HOUR, "frame_overruns", 2.0, "desk"),
    ]);
    s.flush(BASE).unwrap();

    let lake = lake(&root, choice);
    assert_eq!(count(&lake, "metrics"), 3);
    assert_eq!(scalar(&lake, "SELECT count(*) FROM metrics WHERE labels->>'host' = 'laptop'"), 2);
    assert_eq!(scalar(&lake, "SELECT count(*) FROM metrics WHERE hour = 0"), 2);
    assert_eq!(count(&lake, "bench_runs"), 1);
    let st = lake.stats().unwrap();
    assert_eq!(st.tables.iter().map(|t| t.legacy_files).sum::<usize>(), 2);
    assert_eq!(st.tables.iter().find(|t| t.table == "metrics").unwrap().rows, 3);

    // Compaction converts every legacy partition (even single-file ones) to Vortex.
    let r = lake.compact(BASE + 3 * HOUR).unwrap();
    assert!(r.partitions >= 2, "{r:?}");
    assert!(data_files(&root).iter().all(|f| f.extension().is_some_and(|e| e == "vortex")));
    assert_eq!(lake.stats().unwrap().tables.iter().map(|t| t.legacy_files).sum::<usize>(), 0);
    assert_eq!(count(&lake, "metrics"), 3);
    assert_eq!(scalar(&lake, "SELECT count(*) FROM metrics WHERE labels->>'host' = 'laptop'"), 2);
    assert_eq!(scalar(&lake, "SELECT count(*) FROM bench_runs WHERE run_id = 'old-run'"), 1);
    assert_eq!(scalar(&lake, "SELECT count(*) FROM metrics WHERE hour = 0"), 2);
}

both_backends!(legacy_on, legacy_lake_keeps_working_and_compaction_converts_it);
