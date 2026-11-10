//! `cargo xtask otel-report` over **official OTLP JSON Lines** fixtures.
//!
//! The fixtures are the shape the SDK's file exporters write: one top-level object per line
//! (`resourceSpans` / `resourceMetrics` / `resourceLogs`), 64-bit fields as strings, one signal per
//! file. The report must read them out of order and derive its numbers from the spans' exact
//! `endTimeUnixNano - startTimeUnixNano`, not from an invented sample field.

use std::path::{Path, PathBuf};
use std::process::Command;

use deckcraft_telemetry::sc;
use serde_json::{Value, json};

/// The binary under test. Cargo builds it and sets this for every integration test of the package.
fn xtask() -> &'static str {
    env!("CARGO_BIN_EXE_xtask")
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("deckcraft-otel-report-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn attr(key: &str, value: &str) -> Value {
    json!({"key": key, "value": {"stringValue": value}})
}

fn resource(extra: &[(&str, &str)]) -> Value {
    let mut attributes = vec![attr("service.name", "deckcraft"), attr("service.version", "0.15.0")];
    for (key, value) in extra {
        attributes.push(attr(key, value));
    }
    json!({"attributes": attributes})
}

/// One `deckcraft.frame` span, laid out on a cumulative timeline.
fn frame_span(start_ms: f64, duration_ms: f64) -> Value {
    json!({
        "traceId": "AAAAAAAAAAAAAAAAAAAAAA==",
        "spanId": "AAAAAAAAAAA=",
        "name": "deckcraft.frame",
        "kind": 1,
        "startTimeUnixNano": format!("{}", (start_ms * 1e6) as u64),
        "endTimeUnixNano": format!("{}", ((start_ms + duration_ms) * 1e6) as u64),
        "attributes": [],
        "status": {}
    })
}

fn traces_line(resource: &Value, spans: &[Value]) -> String {
    json!({
        "resourceSpans": [{
            "resource": resource,
            "scopeSpans": [{"scope": {"name": "deckcraft-telemetry"}, "spans": spans}]
        }]
    })
    .to_string()
}

fn write_lines(path: &Path, lines: &[String]) {
    std::fs::write(path, format!("{}\n", lines.join("\n"))).expect("write jsonl");
}

/// A histogram metric data point: `bucketCounts` are strings and `count` is a string, as OTLP JSON
/// encodes every 64-bit field.
fn hist_metric(name: &str, unit: &str, count: u64, sum: f64, bounds: &[f64], buckets: &[u64], attrs: &[Value]) -> Value {
    let buckets: Vec<String> = buckets.iter().map(u64::to_string).collect();
    json!({
        "name": name,
        "unit": unit,
        "histogram": {
            "aggregationTemporality": 2,
            "dataPoints": [{
                "attributes": attrs,
                "timeUnixNano": "1000000000",
                "count": count.to_string(),
                "sum": sum,
                "bucketCounts": buckets,
                "explicitBounds": bounds,
            }]
        }
    })
}

fn sum_metric(name: &str, value: u64, attrs: &[Value]) -> Value {
    json!({
        "name": name,
        "unit": "1",
        "sum": {
            "aggregationTemporality": 2,
            "isMonotonic": true,
            "dataPoints": [{"attributes": attrs, "timeUnixNano": "1000000000", "asInt": value.to_string()}]
        }
    })
}

fn metrics_line(resource: &Value, metrics: &[Value]) -> String {
    json!({
        "resourceMetrics": [{
            "resource": resource,
            "scopeMetrics": [{"scope": {"name": "deckcraft-telemetry"}, "metrics": metrics}]
        }]
    })
    .to_string()
}

/// Write a traces-only run: one frame span per duration, on a cumulative timeline.
fn write_run(dir: &Path, variant: &str, adapter: &str, durations: &[f64]) {
    let resource = resource(&[("test.variant", variant), ("test.run_id", "ab"), ("test.zoom", "3.0"), ("hw.gpu.adapter", adapter)]);
    let mut start = 0.0;
    let mut lines = Vec::new();
    for duration in durations {
        lines.push(traces_line(&resource, &[frame_span(start, *duration)]));
        start += duration;
    }
    write_lines(&dir.join("traces.jsonl"), &lines);
}

/// Run the report and return `(exit code, stdout, stderr)`.
fn report(args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(xtask()).args(["otel-report"]).args(args).output().expect("run xtask");
    (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

/// The RED→GREEN slice from the plan: five frame spans of 3, 3, 3, 24 and 26 ms.
///
/// Nearest-rank percentiles: p50 = 3, p90 = p99 = worst = 26. Two frames are over the 16.7 ms
/// budget, but they are consecutive, so they form **one jank episode** — `jank=1`, and the longest
/// run is 2. `jank` is the episode count; the class counters below it count frames.
#[test]
fn five_frame_spans_yield_nearest_rank_percentiles_and_one_jank_episode() {
    let dir = temp_dir("five-frames");
    let mut start = 0.0;
    let mut lines = Vec::new();
    for duration in [3.0, 3.0, 3.0, 24.0, 26.0] {
        lines.push(traces_line(&resource(&[("test.variant", "candidate"), ("test.run_id", "fixture")]), &[frame_span(start, duration)]));
        start += duration;
    }
    write_lines(&dir.join("traces.jsonl"), &lines);

    let (code, stdout, stderr) = report(&[dir.to_str().expect("utf-8 temp path")]);
    assert_eq!(code, Some(0), "the report must exit 0 without a baseline\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("frames: 5"), "the span count is the sample count:\n{stdout}");
    assert!(stdout.contains("p50=3.00ms"), "nearest-rank p50 of [3,3,3,24,26]:\n{stdout}");
    assert!(stdout.contains("p90=26.00ms"), "nearest-rank p90:\n{stdout}");
    assert!(stdout.contains("p99=26.00ms"), "nearest-rank p99:\n{stdout}");
    assert!(stdout.contains("worst=26.00ms"), "worst is the max:\n{stdout}");
    assert!(stdout.contains("jank=1"), "24 and 26 ms are one consecutive jank episode:\n{stdout}");
    assert!(stdout.contains("longest_run=2"), "the episode is two frames long:\n{stdout}");
}

/// An empty directory is not an error: it is the honest report of a run where telemetry was on
/// and nothing was recorded. `frames: 0`, exit 0.
#[test]
fn an_empty_directory_reports_zero_frames_and_exits_zero() {
    let dir = temp_dir("empty");
    let (code, stdout, stderr) = report(&[dir.to_str().expect("utf-8 temp path")]);
    assert_eq!(code, Some(0), "an empty run is valid\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("frames: 0"), "the report says how many frames it saw:\n{stdout}");
    assert!(stdout.contains("p50=n/a"), "no samples means no percentile, never a zero:\n{stdout}");
}

/// Out-of-order lines are legal: the report sorts the frame spans by `startTimeUnixNano` before it
/// derives anything, so a shuffled file yields the same percentiles.
#[test]
fn out_of_order_lines_yield_the_same_percentiles() {
    let dir = temp_dir("shuffled");
    let resource = resource(&[("test.variant", "candidate"), ("test.run_id", "fixture")]);
    let mut start = 0.0;
    let mut lines = Vec::new();
    for duration in [3.0, 3.0, 3.0, 24.0, 26.0] {
        lines.push(traces_line(&resource, &[frame_span(start, duration)]));
        start += duration;
    }
    lines.reverse();
    write_lines(&dir.join("traces.jsonl"), &lines);

    let (code, stdout, _) = report(&[dir.to_str().expect("utf-8 temp path")]);
    assert_eq!(code, Some(0));
    assert!(stdout.contains("p50=3.00ms") && stdout.contains("p90=26.00ms"), "sorting is independent of line order:\n{stdout}");
    assert!(stdout.contains("jank=1") && stdout.contains("longest_run=2"), "the episode is built from start order:\n{stdout}");
}

/// The aggregated metrics fill the lines the spans cannot: cache hit rate, renders per interaction,
/// raster cost per megapixel, upload bandwidth and the health gate. The bucket counts are validated
/// against `count`, not trusted.
#[test]
fn metrics_aggregates_fill_the_cache_raster_and_health_lines() {
    let dir = temp_dir("metrics");
    let resource = resource(&[("test.variant", "candidate"), ("test.run_id", "fixture"), ("test.zoom", "3.0")]);
    let mut start = 0.0;
    let mut traces = Vec::new();
    for duration in [3.0, 3.0, 3.0, 24.0, 26.0] {
        traces.push(traces_line(&resource, &[frame_span(start, duration)]));
        start += duration;
    }
    write_lines(&dir.join("traces.jsonl"), &traces);

    let frame_buckets = [3, 0, 0, 2, 0, 0, 0, 0, 0, 0];
    let raster_buckets = [0, 0, 2, 0, 0, 0, 0, 0, 0, 0];
    let upload_buckets = [2, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let pixel_buckets = [0, 0, 0, 0, 2, 0, 0, 0];
    let byte_buckets = [0, 0, 0, 2, 0, 0, 0, 0];
    let metrics = vec![
        hist_metric(sc::M_FRAME_DURATION, "ms", 5, 59.0, sc::FRAME_BOUNDS_MS, &frame_buckets, &[]),
        sum_metric(sc::M_FRAME_JANKY, 2, &[attr(sc::A_JANK_CLASS, sc::V_JANK_MISSED_VSYNC)]),
        sum_metric(sc::M_CACHE, 3, &[attr(sc::A_CACHE_KIND, sc::V_CANVAS), attr(sc::A_CACHE_RESULT, sc::V_HIT)]),
        sum_metric(sc::M_CACHE, 2, &[attr(sc::A_CACHE_KIND, sc::V_CANVAS), attr(sc::A_CACHE_RESULT, sc::V_MISS)]),
        sum_metric(sc::M_INTERACTION_COUNT, 2, &[]),
        hist_metric(sc::M_RASTER_DURATION, "ms", 2, 20.0, sc::FRAME_BOUNDS_MS, &raster_buckets, &[]),
        hist_metric(sc::M_RASTER_PIXELS, "Mpx", 2, 4.0, sc::PIXEL_BOUNDS, &pixel_buckets, &[]),
        hist_metric(sc::M_TEXTURE_BYTES, "By", 2, 8_000_000.0, sc::BYTE_BOUNDS, &byte_buckets, &[]),
        hist_metric(sc::M_FRAME_PHASE, "ms", 2, 4.0, sc::FRAME_BOUNDS_MS, &upload_buckets, &[attr(sc::A_PHASE, sc::P_UPLOAD)]),
        hist_metric(sc::M_FRAME_PHASE, "ms", 2, 20.0, sc::FRAME_BOUNDS_MS, &raster_buckets, &[attr(sc::A_PHASE, sc::P_RASTER)]),
    ];
    write_lines(&dir.join("metrics.jsonl"), &[metrics_line(&resource, &metrics)]);

    let (code, stdout, stderr) = report(&[dir.to_str().expect("utf-8 temp path")]);
    assert_eq!(code, Some(0), "the aggregates are consistent\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("cache:  hit=3 miss=2  hit_rate=60.0%  renders/interaction=1.00  thrash=0"),
        "cache counters and the raster/interaction ratio:\n{stdout}"
    );
    assert!(stdout.contains("dropped_records=0"), "the histogram count equals the span count:\n{stdout}");
    assert!(stdout.contains("5.00 ms/Mpx"), "20 ms of raster for 4 Mpx:\n{stdout}");
    assert!(stdout.contains("2.00 GB/s"), "8 MB uploaded in 4 ms:\n{stdout}");
    assert!(stdout.contains("raster") && stdout.contains("16.61 ms"), "the phase table is built from the histogram:\n{stdout}");
}

/// A/B on identical environments: the candidate is the same build, so p50 must be 1.00x and the
/// report must print the PASS line. The baseline/candidate durations are the plan's 5-frame fixture
/// versus an all-3 ms run, which makes the p99 ratio unambiguous.
#[test]
fn a_paired_ab_reports_ratios_and_passes_within_the_gates() {
    let candidate = temp_dir("ab-candidate-pass");
    let baseline = temp_dir("ab-baseline-pass");
    write_run(&candidate, "candidate", "llvmpipe", &[3.0, 3.0, 3.0, 3.0, 3.0]);
    write_run(&baseline, "baseline", "llvmpipe", &[3.0, 3.0, 3.0, 24.0, 26.0]);

    let (code, stdout, stderr) = report(&[candidate.to_str().expect("utf-8 path"), "--baseline", baseline.to_str().expect("utf-8 path")]);
    assert_eq!(code, Some(0), "the candidate is faster\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("A/B (candidate/baseline): p50 1.00x  p99 0.12x"), "the ratios are candidate/baseline:\n{stdout}");
    assert!(stdout.contains("PASS: p99 within gate (<=1.10x), median within gate (<=1.05x)"), "the gate passes:\n{stdout}");
}

/// A regression is a non-zero exit and a FAIL line naming the gate it broke. The candidate is
/// 33% slower at the median and 10x at p99, so both gates must fire.
#[test]
fn a_slower_candidate_fails_the_p50_and_p99_gates() {
    let candidate = temp_dir("ab-candidate-fail");
    let baseline = temp_dir("ab-baseline-fail");
    write_run(&candidate, "candidate", "llvmpipe", &[4.0, 4.0, 4.0, 30.0, 30.0]);
    write_run(&baseline, "baseline", "llvmpipe", &[3.0, 3.0, 3.0, 3.0, 3.0]);

    let (code, stdout, stderr) = report(&[candidate.to_str().expect("utf-8 path"), "--baseline", baseline.to_str().expect("utf-8 path")]);
    assert_ne!(code, Some(0), "a regression must not exit 0\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("FAIL: p99 10.00x > 1.10x"), "the p99 gate names the ratio:\n{stdout}");
    assert!(stdout.contains("FAIL: median 1.33x > 1.05x"), "the median gate names the ratio:\n{stdout}");
}

/// Two runs from different GPUs are not comparable: the report refuses before it computes a ratio
/// that would look like a code regression.
#[test]
fn different_gpu_adapters_are_refused_as_an_ab_baseline() {
    let candidate = temp_dir("ab-candidate-gpu");
    let baseline = temp_dir("ab-baseline-gpu");
    write_run(&candidate, "candidate", "NVIDIA GeForce MX450", &[3.0, 3.0, 3.0, 3.0, 3.0]);
    write_run(&baseline, "baseline", "llvmpipe", &[3.0, 3.0, 3.0, 3.0, 3.0]);

    let (code, stdout, stderr) = report(&[candidate.to_str().expect("utf-8 path"), "--baseline", baseline.to_str().expect("utf-8 path")]);
    assert_ne!(code, Some(0), "a cross-adapter comparison is refused\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("REFUSED: resource mismatch (hw.gpu.adapter: candidate=NVIDIA GeForce MX450 baseline=llvmpipe)"),
        "the refusal names the attribute and both values:\n{stdout}"
    );
}

/// A malformed line is a failed measurement, not a number to skip: the report names the file and
/// the line and exits non-zero.
#[test]
fn a_malformed_line_names_the_file_and_line() {
    let dir = temp_dir("malformed");
    std::fs::write(dir.join("traces.jsonl"), "{\"resourceSpans\": [}\n").expect("write");
    let (code, stdout, stderr) = report(&[dir.to_str().expect("utf-8 temp path")]);
    assert_ne!(code, Some(0), "malformed JSON must fail\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stderr.contains("traces.jsonl:1"), "the error names the file and line:\n{stderr}");
}

/// A histogram whose buckets do not add up to its count is internally inconsistent; averaging it
/// would turn a corrupt aggregate into an authoritative-looking percentile.
#[test]
fn an_inconsistent_histogram_is_rejected() {
    let dir = temp_dir("bad-histogram");
    let resource = resource(&[("test.variant", "candidate")]);
    let metrics = vec![hist_metric(sc::M_FRAME_DURATION, "ms", 5, 59.0, sc::FRAME_BOUNDS_MS, &[3, 0, 0, 1, 0, 0, 0, 0, 0, 0], &[])];
    write_lines(&dir.join("metrics.jsonl"), &[metrics_line(&resource, &metrics)]);
    let (code, stdout, stderr) = report(&[dir.to_str().expect("utf-8 temp path")]);
    assert_ne!(code, Some(0), "an inconsistent histogram must fail\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stderr.contains("invalid histogram") && stderr.contains("bucketCounts sum 4 != count 5"), "the error explains the mismatch:\n{stderr}");
}
