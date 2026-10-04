//! Builds a realistic compacted lake for comparing Vortex with the old Parquet format.
//! Ignored by default; run with
//! `cargo test -p lindesk-query --test compare -- --ignored --nocapture`.

mod common;

use common::*;
use lindesk_query::BackendChoice;

#[test]
#[ignore = "writes a large lake for a manual format comparison"]
fn build_comparison_lake() {
    let root = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("compare-lake/telemetry");
    let _ = std::fs::remove_dir_all(&root);
    let mut s = store(&root);
    for h in 0..6 {
        s.append(&mix(BASE + h * HOUR, 1, 1));
        s.flush(BASE).unwrap();
    }
    let lake = lake(&root, BackendChoice::Auto);
    let r = lake.compact(BASE + 7 * HOUR).unwrap();
    eprintln!("lake at {} ({r:?})", root.display());
    eprintln!(
        "{}",
        lake.query(
            "SELECT 'pressure', count(*) FROM pressure UNION ALL SELECT 'units', count(*) FROM units",
            &lindesk_query::Format::Table
        )
        .unwrap()
    );
}
