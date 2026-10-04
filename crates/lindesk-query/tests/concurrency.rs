//! Readers run `count(*)` in a loop while a writer appends and maintenance compacts the
//! very partitions being read. Every row carries a unique id, so a reader that saw a
//! partition's old and new files at once would count duplicates.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use common::*;
use lindesk_plugin_api::{Record, Resource};
use lindesk_query::{BackendChoice, Format, Lake};

fn snapshot(lake: &Lake) -> (i64, i64) {
    let out = lake
        .query("SELECT count(*), count(DISTINCT some_delta_us) FROM pressure", &Format::Csv)
        .expect("query must succeed (after retries)");
    let v: Vec<i64> = out.lines().nth(1).unwrap().split(',').map(|v| v.parse().unwrap()).collect();
    (v[0], v[1])
}

fn queries_never_double_count_during_compaction(choice: BackendChoice) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("telemetry");
    let mut store = store(&root);
    let lake = lake(&root, choice);
    // Every partition written is sealed relative to this "now".
    let now = BASE + 10 * HOUR;

    let attempted = Arc::new(AtomicI64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let swaps = Arc::new(AtomicUsize::new(0));
    // Flushes completed by the writer; the compactor waits for a new one between runs.
    // Writers and maintenance exclude each other through the lake lock by design, and
    // flock(2) is not fair, so without this a tight compaction loop could starve the
    // writer on a heavily loaded machine. Readers are never synchronised.
    let flushes = Arc::new(AtomicUsize::new(0));
    const MIN_SWAPS: usize = 12;

    let writer = {
        let attempted = attempted.clone();
        let swaps = swaps.clone();
        let flushes = flushes.clone();
        thread::spawn(move || {
            let mut id: u64 = 0;
            let mut round = 0;
            // Keep writing until compaction has swapped partitions under the readers
            // several times (bounded, in case the machine is very slow).
            while (round < 20 || swaps.load(Ordering::SeqCst) < MIN_SWAPS) && round < 300 {
                round += 1;
                let batch: Vec<Record> = (0..50)
                    .map(|i| {
                        id += 1;
                        // Spread rows over three sealed hours so compaction keeps swapping
                        // partitions that readers are scanning.
                        let ts = BASE + ((round + i) % 3) as i64 * HOUR + (id as i64 % 3_600) * 1000;
                        pressure(ts, "system", Resource::Io, 1.0, id)
                    })
                    .collect();
                // Count before the rows can become visible, so readers never see more
                // than `attempted`.
                attempted.fetch_add(batch.len() as i64, Ordering::SeqCst);
                store.append(&batch);
                // Wait for a gap between compactions so writes interleave with swaps.
                let r = store.flush_wait(now, Duration::from_secs(300)).unwrap();
                assert_eq!(r.deferred_rows, 0);
                flushes.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(3));
            }
            assert_eq!(store.buffered_rows(), 0);
        })
    };

    let compactor = {
        let lake = lake.clone();
        let done = done.clone();
        let swaps = swaps.clone();
        let flushes = flushes.clone();
        thread::spawn(move || {
            let mut seen = 0;
            while !done.load(Ordering::SeqCst) {
                let current = flushes.load(Ordering::SeqCst);
                if current == seen {
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                seen = current;
                swaps.fetch_add(lake.compact(now).unwrap().partitions, Ordering::SeqCst);
            }
        })
    };

    let readers: Vec<_> = (0..2)
        .map(|_| {
            let lake = lake.clone();
            let attempted = attempted.clone();
            let done = done.clone();
            thread::spawn(move || {
                let mut last = 0;
                let mut queries = 0;
                while !done.load(Ordering::SeqCst) {
                    let (n, distinct) = snapshot(&lake);
                    let upper = attempted.load(Ordering::SeqCst);
                    assert_eq!(n, distinct, "a row was counted twice");
                    assert!(n <= upper, "counted {n} rows but only {upper} were written");
                    assert!(n >= last, "count went backwards: {last} -> {n}");
                    last = n;
                    queries += 1;
                }
                queries
            })
        })
        .collect();

    let started = Instant::now();
    writer.join().unwrap();
    done.store(true, Ordering::SeqCst);
    compactor.join().unwrap();
    let swaps = swaps.load(Ordering::SeqCst);
    let queries: usize = readers.into_iter().map(|r| r.join().unwrap()).sum();
    assert!(swaps >= MIN_SWAPS, "compaction swapped only {swaps} partitions");
    assert!(queries > 0, "no queries ran");
    eprintln!("{queries} queries, {swaps} partition swaps in {:?}", started.elapsed());

    let total = attempted.load(Ordering::SeqCst);
    assert_eq!(snapshot(&lake), (total, total));
    lake.compact(now).unwrap();
    assert_eq!(snapshot(&lake), (total, total));
    assert_eq!(scalar(&lake, "SELECT sum(samples) FROM pressure_1h"), total);
    assert_eq!(data_files(&root.join("pressure")).len(), 3);
}

both_backends!(concurrency, queries_never_double_count_during_compaction);
