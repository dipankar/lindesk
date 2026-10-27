#!/usr/bin/env python3
"""Summarize legacy Parquet benchmarks and exported latency rings, without modifying the lake.

Requires pyarrow for reading legacy benchmark Parquet. No network, daemon mutations,
third-party assets, or installs. Example:
  python3 scripts/analyze-local-data.py --lake ~/.local/state/lindesk/telemetry \
      --snapshot-dir /tmp --out dist/analysis-2026-10-09

Snapshot filenames: lindesk-analysis-{session,broker}-{status,latency}.json.
Vortex is deliberately not silently treated as Parquet; unsupported files are reported.
"""
import argparse
import datetime as dt
import hashlib
import html
import json
import math
from pathlib import Path


def percentile(values, p):
    """Nearest rank of raw observations; never pool already aggregated percentiles."""
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * p) - 1)] if ordered else None


def labels(value):
    if isinstance(value, str):
        value = json.loads(value)
    return dict(value or [])


def load_benchmarks(root):
    import pyarrow.parquet as pq
    tables, sources, unsupported = {}, [], []
    for name in ("bench_runs", "bench_samples"):
        rows = []
        for path in sorted((root / name).rglob("*.parquet")):
            rows.extend(pq.ParquetFile(path).read().to_pylist())
            sources.append({"path": str(path), "bytes": path.stat().st_size,
                            "sha256": hashlib.sha256(path.read_bytes()).hexdigest()})
        unsupported.extend(str(p) for p in (root / name).rglob("*.vortex"))
        tables[name] = rows
    output = []
    for run in sorted(tables["bench_runs"], key=lambda r: (r["ts"], r["run_id"])):
        config = json.loads(run["config"])
        samples = [s for s in tables["bench_samples"] if s["run_id"] == run["run_id"]]
        for phase in sorted({s["phase"] for s in samples}):
            summary = [s for s in samples if s["phase"] == phase and s.get("iteration") is None]
            def value(metric, **match):
                found = [s["value"] for s in summary if s["metric"] == metric
                         and all(labels(s.get("labels")).get(k) == v for k, v in match.items())]
                # Duplicate summaries would need grouping by repetition: do not average them.
                return found[0] if len(found) == 1 else None
            def millis(metric, stat):
                n = value(metric, stat=stat)
                return n / 1000 if n is not None else None
            output.append({
                "run_id": run["run_id"], "time_utc": run["ts"].isoformat(), "phase_label": phase,
                "duration_s": config.get("config", {}).get("duration_s"),
                "configured_phases": config.get("config", {}).get("phases"),
                "service_state_managed": config.get("config", {}).get("manage_services"),
                "storage_at_run": config.get("host", {}).get("scratch_fs"),
                "wakeup_p99_ms": millis("wakeup_overshoot_us", "p99"),
                "read_p99_ms": millis("read_latency_us", "p99"),
                "read_max_ms": millis("read_latency_us", "max"),
                "read_count": value("read_latency_us", stat="count"),
                "fault_p99_ms": millis("fault_latency_us", "p99"),
                "probe_io_stall_pct": value("psi_some_pct", resource="io", scope="probe"),
                "system_io_stall_pct": value("psi_some_pct", resource="io", scope="system"),
                "system_memory_stall_pct": value("psi_some_pct", resource="memory", scope="system"),
            })
    return output, sources, unsupported, len(tables["bench_samples"])


def load_live(directory):
    live = {}
    for name in ("session", "broker"):
        status_path = directory / f"lindesk-analysis-{name}-status.json"
        latency_path = directory / f"lindesk-analysis-{name}-latency.json"
        if not status_path.exists() or not latency_path.exists():
            continue
        status, latency = (json.loads(p.read_text()) for p in (status_path, latency_path))
        daemon = status.get("daemon", {})
        contexts = latency.get("context", [])
        costs = [c["timings_us"]["total_us"] / 1000 for c in contexts]
        live[name] = {
            "version": daemon.get("version"), "state": daemon.get("state"), "mode": daemon.get("mode"),
            "telemetry": daemon.get("telemetry"), "steps_in_export": len(contexts),
            "from_ms": contexts[0]["ts"] if contexts else None,
            "to_ms": contexts[-1]["ts"] if contexts else None,
            "engine_p50_ms": percentile(costs, .5), "engine_p99_ms": percentile(costs, .99),
            "engine_max_ms": max(costs, default=None),
            "application_metrics": latency.get("summary", {}),
            "context": contexts,
        }
    return live


def cell(value):
    if value is None:
        return "unknown"
    if isinstance(value, float):
        return f"{value:,.3f}"
    return html.escape(str(value))


def render(report):
    rows = "".join("<tr>" + "".join(f"<td>{cell(r[k])}</td>" for k in (
        "time_utc", "phase_label", "read_count", "wakeup_p99_ms", "read_p99_ms", "read_max_ms",
        "probe_io_stall_pct", "system_io_stall_pct")) + "</tr>" for r in report["benchmarks"])
    live_rows = "".join(f"<tr><td>{name}</td>" + "".join(f"<td>{cell(r.get(k))}</td>" for k in (
        "version", "state", "steps_in_export", "engine_p50_ms", "engine_p99_ms", "engine_max_ms"))
        + "</tr>" for name, r in report["live"].items())
    metrics = sum(len(r["application_metrics"]) for r in report["live"].values())
    telemetry = {name: r["telemetry"] for name, r in report["live"].items()}
    return f'''<!doctype html><html lang="en"><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>Lindesk: local data analysis</title>
<style>body{{font:16px/1.6 system-ui,sans-serif;color:#19334a;background:#f3f6f9;margin:0}}
main{{max-width:1150px;margin:auto;padding:40px 24px}}h1{{font-size:34px;line-height:1.2}}h2{{margin-top:32px}}
section{{background:white;padding:24px;margin:20px 0;border:1px solid #d7e0e8;border-radius:12px}}
.scroll{{overflow:auto}}table{{width:100%;border-collapse:collapse;font-size:14px}}th,td{{text-align:left;padding:10px;border-bottom:1px solid #e5eaf0;white-space:nowrap}}
th{{color:#355b73}}code,pre{{background:#edf2f7}}pre{{padding:14px;overflow:auto}}.note{{border-left:4px solid #c28b35;padding-left:14px}}a{{color:#125e93}}</style>
<main><p>LOCAL ANALYSIS · {cell(report["generated_utc"])}</p><h1>What the recorded data can tell us</h1>
<p>Historical synthetic benchmarks and current engine diagnostics are presented separately. No system tuning was performed.</p>
<section><h2>Historical benchmark evidence</h2><p>{len(report["sources"])} source files; {report["summary_rows"]} stored metric rows. These rows include aggregate statistics and timelines, not individual read operations.</p>
<div class="scroll"><table><thead><tr><th>Start UTC</th><th>Phase label</th><th>Read samples</th><th>Wakeup p99 ms</th><th>Read p99 ms</th><th>Read maximum ms</th><th>Probe I/O PSI %</th><th>System I/O PSI %</th></tr></thead><tbody>{rows}</tbody></table></div>
<p class="note">All three runs in the current dataset are short, October 4 synthetic-load tests labelled lindesk_off. There are no paired on-phase results. The stored configuration says service management was disabled; the label alone does not establish the daemon's actual state.</p>
<p>The read-delay distributions and probe I/O stall measurements make storage contention the strongest lead in these tests. They do not demonstrate a defective drive or establish the cause of a particular desktop stutter. The second run also shows why high system PSI alone is insufficient: system I/O pressure was high while the probe experienced much less I/O stall.</p>
<p>Do not average these p99 values into a combined percentile. Read sample counts range from 196 to 1,001, making the extreme tails especially sensitive to a few operations. These historical runs used BFQ; the current host status reports Kyber, so historical and current results are not directly comparable.</p></section>
<section><h2>Current engine diagnostics</h2><div class="scroll"><table><thead><tr><th>Service</th><th>Version</th><th>State</th><th>Steps in buffer</th><th>Engine median ms</th><th>Engine p99 ms</th><th>Engine maximum ms</th></tr></thead><tbody>{live_rows}</tbody></table></div>
<p>These are engine-step wall times, including waits, not desktop input latency and not CPU overhead percentages. Each buffer is capped at 240 steps; the JSON records its exact time window. Startup, user actions, and current workload can affect this short sample.</p>
<p>Application metric series present: <strong>{metrics}</strong>. With no frame, input, or audio observations, this dataset cannot establish smoother presentation or fewer audio dropouts.</p>
<details><summary>Recording status</summary><pre>{html.escape(json.dumps(telemetry,indent=2))}</pre></details>
<p>Core Lindesk is on. The optional continuous-telemetry component is not installed in the inspected status; enabled=true in configuration alone does not install a writer. The protected system lake could not be read by this account and was not included.</p></section>
<section><h2>Next useful experiment</h2><ol><li>Collect equivalent off/on/on/off phases on the current version and unchanged storage settings. Keep background throughput and duration alongside latency so reduced work is not mistaken for an improvement.</li><li>Separate CPU-only, storage-only, and mixed-load runs to isolate the likely I/O bottleneck. Record actual application frame/input timing or audio errors alongside synthetic probes.</li><li>Use continuous telemetry for multi-hour workload correlation after installing the optional add-on; until then, export the in-memory histories before restarting services.</li></ol>
<p>A modest initial synthetic comparison, when ready to generate load and briefly pause/resume Lindesk:</p><pre>lindesk-bench run --quick --repeats 2 --cpu 4 --no-mem \
  --write-cap 256M --dir /tmp/lindesk-analysis-bench \
  --out ./bench-results --label storage-investigation</pre>
<p>This command is a proposed next step; it was not executed for this report. A synthetic probe does not measure real frame delivery. Repeat in desktop-focus mode with application instrumentation before drawing desktop-experience conclusions.</p></section>
<p><a href="analysis.json">Analysis data, source hashes, and context samples</a> · <a href="benchmark-summary.csv">Benchmark summary CSV</a></p></main></html>'''


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--lake", type=Path, required=True)
    parser.add_argument("--snapshot-dir", type=Path, default=Path("/tmp"))
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    benchmarks, sources, unsupported, count = load_benchmarks(args.lake)
    if unsupported:
        raise SystemExit("Vortex benchmark partitions present; export them with lindesk-data before using this legacy Parquet analysis: " + ", ".join(unsupported))
    # This report's findings describe the three known legacy baseline runs; avoid silently
    # reusing those conclusions for new datasets with on phases or another test design.
    if len(benchmarks) != 3 or any(r["phase_label"] != "lindesk_off" or not r["time_utc"].startswith("2026-10-04") for r in benchmarks):
        raise SystemExit("This analysis targets the three October 4 baseline runs; revise the findings for this dataset.")
    report = {"generated_utc": dt.datetime.now(dt.timezone.utc).isoformat(), "lake": str(args.lake),
              "sources": sources, "summary_rows": count, "benchmarks": benchmarks, "live": load_live(args.snapshot_dir)}
    args.out.mkdir(parents=True, exist_ok=True)
    (args.out / "analysis.json").write_text(json.dumps(report, indent=2, default=str) + "\n")
    (args.out / "index.html").write_text(render(report))
    import csv
    with (args.out / "benchmark-summary.csv").open("w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=list(benchmarks[0])); writer.writeheader(); writer.writerows(benchmarks)
    print(args.out / "index.html")
    for r in benchmarks:
        print(r["time_utc"], "read p99 ms:", r["read_p99_ms"], "read max ms:", r["read_max_ms"])


if __name__ == "__main__":
    main()
