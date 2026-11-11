//! `cargo xtask otel-report`: aggregate **official OTLP JSON Lines** into a bottleneck + A/B report.
//!
//! # Input
//!
//! The [OTLP File Exporter] format: one file per signal (`traces.jsonl`, `metrics.jsonl`,
//! `logs.jsonl`), UTF-8, one top-level object per line (`TracesData` / `MetricsData` /
//! `LogsData`), no ordering guarantee. The report therefore accumulates every line and sorts the
//! frame spans by `startTimeUnixNano` before deriving anything.
//!
//! Metrics arrive **aggregated** (`bucketCounts` + `explicitBounds` + `sum` + `count`). Exact
//! per-frame samples come from the spans: `endTimeUnixNano - startTimeUnixNano`. A histogram can
//! answer "how many frames missed 16.7 ms" but not "what was the worst frame"; the spans can, and
//! that is why the frame percentiles are computed from them and the histogram is only validated
//! and used where no span exists (the phase breakdown, whose raster/convert/upload phases are
//! recorded as instruments).
//!
//! [OTLP File Exporter]: https://opentelemetry.io/docs/specs/otel/protocol/file-exporter/

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use deckcraft_telemetry::sc;
use serde_json::Value;

/// Exit gates, from `docs/egui-037-migration-plan.md` §8.4.
const P99_GATE: f64 = 1.10;
const P50_GATE: f64 = 1.05;
/// §8.3: a median whose run-to-run CV exceeds this is too noisy to compare.
const CV_GATE: f64 = 0.03;
/// Below this sample count the CV is not estimated: §8.3 asks for at least 1,000 frames.
const MIN_CV_FRAMES: usize = 1_000;
/// Bootstrap resamples for the median's relative standard error. Fixed seed: a report must be
/// reproducible byte for byte from the same input.
const BOOTSTRAP_RESAMPLES: usize = 256;

/// Resource attributes that must match before two runs may be compared. A different GPU adapter,
/// DPI or renderer backend makes a ratio a comparison of machines, not of builds.
const COMPARABLE_RESOURCE_KEYS: &[&str] = &[
    sc::A_HW_GPU_ADAPTER,
    sc::A_RENDERER_BACKEND,
    sc::A_DEVICE_PIXEL_RATIO,
    sc::A_DISPLAY_SUBSYSTEM,
    sc::A_DISPLAY_REFRESH_HZ,
    sc::A_DISPLAY_VSYNC,
    sc::A_BUILD_PROFILE,
    sc::A_TEST_ZOOM,
];

/// One OTLP attribute value, in the subset the report reads.
#[derive(Clone, Debug, PartialEq)]
enum Attr {
    Str(String),
    Int(i64),
    Double(f64),
    Bool(bool),
}

impl Attr {
    fn as_str(&self) -> Option<&str> {
        match self {
            Attr::Str(value) => Some(value),
            _ => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Attr::Double(value) => Some(*value),
            Attr::Int(value) => Some(*value as f64),
            Attr::Str(value) => value.parse().ok(),
            Attr::Bool(_) => None,
        }
    }

    fn display(&self) -> String {
        match self {
            Attr::Str(value) => value.clone(),
            Attr::Int(value) => value.to_string(),
            Attr::Double(value) => format!("{value}"),
            Attr::Bool(value) => value.to_string(),
        }
    }
}

type Attributes = BTreeMap<String, Attr>;

fn parse_attributes(value: Option<&Value>) -> Attributes {
    let mut out = Attributes::new();
    let Some(list) = value.and_then(Value::as_array) else {
        return out;
    };
    for item in list {
        let Some(key) = item.get("key").and_then(Value::as_str) else {
            continue;
        };
        let Some(inner) = item.get("value") else {
            continue;
        };
        if let Some(value) = inner.get("stringValue").and_then(Value::as_str) {
            out.insert(key.to_owned(), Attr::Str(value.to_owned()));
        } else if let Some(value) = inner.get("intValue").and_then(parse_i64) {
            out.insert(key.to_owned(), Attr::Int(value));
        } else if let Some(value) = inner.get("doubleValue").and_then(Value::as_f64) {
            out.insert(key.to_owned(), Attr::Double(value));
        } else if let Some(value) = inner.get("boolValue").and_then(Value::as_bool) {
            out.insert(key.to_owned(), Attr::Bool(value));
        }
    }
    out
}

/// A 64-bit field: OTLP JSON encodes it as a string, but a hand-written fixture may use a number.
fn parse_u64(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

fn parse_i64(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}

fn parse_f64(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str()?.parse().ok()).filter(|v| v.is_finite())
}

fn series_key(name: &str, attrs: &Attributes) -> String {
    let mut out = String::with_capacity(name.len() + 16);
    out.push_str(name);
    out.push('|');
    for (key, value) in attrs {
        out.push_str(key);
        out.push('=');
        out.push_str(&value.display());
        out.push(',');
    }
    out
}

fn duration_ms(span: &Span) -> f64 {
    (span.end_ns.saturating_sub(span.start_ns)) as f64 / 1.0e6
}

fn time_of(point: &Value) -> u64 {
    point.get("timeUnixNano").and_then(parse_u64).unwrap_or(0)
}

fn temporality(value: &Value) -> u8 {
    value.get("aggregationTemporality").and_then(parse_u64).unwrap_or(2) as u8
}

fn datapoints(value: &Value) -> &[Value] {
    value.get("dataPoints").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

#[derive(Clone, Debug)]
struct SpanEvent {
    name: String,
    attrs: Attributes,
}

#[derive(Clone, Debug)]
struct Span {
    name: String,
    start_ns: u64,
    end_ns: u64,
    attrs: Attributes,
    events: Vec<SpanEvent>,
}

#[derive(Clone, Debug)]
enum MetricValue {
    Hist { count: u64, sum: f64, buckets: Vec<u64>, bounds: Vec<f64> },
    Sum(f64),
    Gauge(f64),
}

#[derive(Clone, Debug)]
struct MetricPoint {
    name: String,
    attrs: Attributes,
    value: MetricValue,
    time_ns: u64,
    delta: bool,
}

impl MetricPoint {
    fn key(&self) -> String {
        series_key(&self.name, &self.attrs)
    }

    /// Accumulate a delta-temporality sample. Cumulative points are never merged: the last
    /// snapshot already contains the run's total, and adding snapshots would double-count.
    fn merge_delta(&mut self, other: &MetricPoint) {
        match (&mut self.value, &other.value) {
            (MetricValue::Hist { count, sum, buckets, .. }, MetricValue::Hist { count: c, sum: s, buckets: b, .. }) => {
                *count = count.saturating_add(*c);
                *sum += s;
                for (left, right) in buckets.iter_mut().zip(b) {
                    *left = left.saturating_add(*right);
                }
            }
            (MetricValue::Sum(left), MetricValue::Sum(right)) => *left += right,
            (MetricValue::Gauge(left), MetricValue::Gauge(right)) => *left = *right,
            _ => {}
        }
    }
}

#[derive(Debug, Default)]
struct Run {
    spans: Vec<Span>,
    metrics: Vec<MetricPoint>,
    resource: Attributes,
    log_errors: usize,
    notes: Vec<String>,
}

impl Run {
    fn load(dir: &Path) -> Result<Run, String> {
        let mut run = Run::default();
        if !dir.exists() {
            run.notes.push(format!("{} does not exist; reporting an empty run", dir.display()));
            return Ok(run);
        }
        for file in ["traces.jsonl", "metrics.jsonl", "logs.jsonl"] {
            let path = dir.join(file);
            if !path.exists() {
                continue;
            }
            let text = std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
            for (index, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let number = index + 1;
                let value: Value = serde_json::from_str(line).map_err(|error| format!("{}:{number}: {error}", path.display()))?;
                run.parse_record(&value, &path, number)?;
            }
        }
        Ok(run)
    }

    fn parse_record(&mut self, value: &Value, path: &Path, line: usize) -> Result<(), String> {
        if value.get("resourceSpans").is_some() {
            self.parse_spans(value, path, line)
        } else if value.get("resourceMetrics").is_some() {
            self.parse_metrics(value, path, line)
        } else if value.get("resourceLogs").is_some() {
            self.parse_logs(value);
            Ok(())
        } else {
            Err(format!("{}:{line}: not an OTLP signal (expected resourceSpans, resourceMetrics or resourceLogs)", path.display()))
        }
    }

    fn parse_spans(&mut self, root: &Value, path: &Path, line: usize) -> Result<(), String> {
        let Some(resource_spans) = root.get("resourceSpans").and_then(Value::as_array) else {
            return Ok(());
        };
        for resource_span in resource_spans {
            self.merge_resource(resource_span.get("resource"));
            let Some(scope_spans) = resource_span.get("scopeSpans").and_then(Value::as_array) else {
                continue;
            };
            for scope_span in scope_spans {
                let Some(spans) = scope_span.get("spans").and_then(Value::as_array) else {
                    continue;
                };
                for span in spans {
                    self.push_span(span, path, line)?;
                }
            }
        }
        Ok(())
    }

    fn push_span(&mut self, span: &Value, path: &Path, line: usize) -> Result<(), String> {
        let Some(name) = span.get("name").and_then(Value::as_str) else {
            return Ok(());
        };
        let start = span.get("startTimeUnixNano").and_then(parse_u64);
        let end = span.get("endTimeUnixNano").and_then(parse_u64);
        let (Some(start_ns), Some(end_ns)) = (start, end) else {
            return Err(format!("{}:{line}: span `{name}` has no start/end time", path.display()));
        };
        let events = span
            .get("events")
            .and_then(Value::as_array)
            .map(|events| {
                events
                    .iter()
                    .filter_map(|event| {
                        Some(SpanEvent {
                            name: event.get("name").and_then(Value::as_str)?.to_owned(),
                            attrs: parse_attributes(event.get("attributes")),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.spans.push(Span {
            name: name.to_owned(),
            start_ns,
            end_ns: end_ns.max(start_ns),
            attrs: parse_attributes(span.get("attributes")),
            events,
        });
        Ok(())
    }

    fn parse_metrics(&mut self, root: &Value, path: &Path, line: usize) -> Result<(), String> {
        let Some(resource_metrics) = root.get("resourceMetrics").and_then(Value::as_array) else {
            return Ok(());
        };
        for resource_metric in resource_metrics {
            self.merge_resource(resource_metric.get("resource"));
            let Some(scope_metrics) = resource_metric.get("scopeMetrics").and_then(Value::as_array) else {
                continue;
            };
            for scope_metric in scope_metrics {
                let Some(metrics) = scope_metric.get("metrics").and_then(Value::as_array) else {
                    continue;
                };
                for metric in metrics {
                    self.push_metric(metric, path, line)?;
                }
            }
        }
        Ok(())
    }

    fn push_metric(&mut self, metric: &Value, path: &Path, line: usize) -> Result<(), String> {
        let Some(name) = metric.get("name").and_then(Value::as_str) else {
            return Ok(());
        };
        if let Some(histogram) = metric.get("histogram") {
            return self.push_histogram(name, histogram, path, line);
        }
        if let Some(sum) = metric.get("sum") {
            return self.push_sum(name, sum, path, line);
        }
        if let Some(gauge) = metric.get("gauge") {
            self.push_gauge(name, gauge);
        }
        Ok(())
    }

    fn push_histogram(&mut self, name: &str, histogram: &Value, path: &Path, line: usize) -> Result<(), String> {
        let delta = temporality(histogram) == 1;
        for point in datapoints(histogram) {
            let count =
                point.get("count").and_then(parse_u64).ok_or_else(|| format!("{}:{line}: histogram `{name}` has no count", path.display()))?;
            let sum = point.get("sum").and_then(parse_f64).unwrap_or(0.0);
            let bounds = point
                .get("explicitBounds")
                .and_then(Value::as_array)
                .map(|values| values.iter().filter_map(parse_f64).collect::<Vec<f64>>())
                .unwrap_or_default();
            let buckets = point
                .get("bucketCounts")
                .and_then(Value::as_array)
                .map(|values| values.iter().filter_map(parse_u64).collect::<Vec<u64>>())
                .unwrap_or_default();
            validate_histogram(name, count, &buckets, &bounds, path, line)?;
            self.metrics.push(MetricPoint {
                name: name.to_owned(),
                attrs: parse_attributes(point.get("attributes")),
                value: MetricValue::Hist { count, sum, buckets, bounds },
                time_ns: time_of(point),
                delta,
            });
        }
        Ok(())
    }

    fn push_sum(&mut self, name: &str, sum: &Value, path: &Path, line: usize) -> Result<(), String> {
        let delta = temporality(sum) == 1;
        for point in datapoints(sum) {
            let value = point.get("asInt").and_then(parse_f64).or_else(|| point.get("asDouble").and_then(parse_f64));
            let Some(value) = value else {
                return Err(format!("{}:{line}: sum `{name}` has no asInt/asDouble", path.display()));
            };
            self.metrics.push(MetricPoint {
                name: name.to_owned(),
                attrs: parse_attributes(point.get("attributes")),
                value: MetricValue::Sum(value),
                time_ns: time_of(point),
                delta,
            });
        }
        Ok(())
    }

    fn push_gauge(&mut self, name: &str, gauge: &Value) {
        for point in datapoints(gauge) {
            let value = point.get("asInt").and_then(parse_f64).or_else(|| point.get("asDouble").and_then(parse_f64)).unwrap_or(0.0);
            self.metrics.push(MetricPoint {
                name: name.to_owned(),
                attrs: parse_attributes(point.get("attributes")),
                value: MetricValue::Gauge(value),
                time_ns: time_of(point),
                delta: false,
            });
        }
    }

    fn parse_logs(&mut self, root: &Value) {
        let Some(resource_logs) = root.get("resourceLogs").and_then(Value::as_array) else {
            return;
        };
        for resource_log in resource_logs {
            self.merge_resource(resource_log.get("resource"));
            let Some(scope_logs) = resource_log.get("scopeLogs").and_then(Value::as_array) else {
                continue;
            };
            for scope_log in scope_logs {
                let Some(records) = scope_log.get("logRecords").and_then(Value::as_array) else {
                    continue;
                };
                for record in records {
                    if record.get("severityNumber").and_then(parse_u64).unwrap_or(0) >= 17 {
                        self.log_errors += 1;
                    }
                }
            }
        }
    }

    fn merge_resource(&mut self, resource: Option<&Value>) {
        let Some(attributes) = resource.and_then(|value| value.get("attributes")) else {
            return;
        };
        for (key, value) in parse_attributes(Some(attributes)) {
            match self.resource.get(&key) {
                Some(existing) if *existing != value => {
                    self.notes.push(format!("resource `{key}` differs between records: {} vs {}", existing.display(), value.display()));
                }
                Some(_) => {}
                None => {
                    self.resource.insert(key, value);
                }
            }
        }
    }
}

/// A histogram must be internally consistent: one bucket more than bounds, every sample counted,
/// strictly increasing finite bounds. A malformed aggregate is a failed measurement, not a number
/// to average.
fn validate_histogram(name: &str, count: u64, buckets: &[u64], bounds: &[f64], path: &Path, line: usize) -> Result<(), String> {
    if count == 0 && buckets.is_empty() {
        return Ok(());
    }
    if buckets.len() != bounds.len() + 1 {
        return Err(format!(
            "{}:{line}: invalid histogram `{name}`: {} bucketCounts for {} explicitBounds (expected {})",
            path.display(),
            buckets.len(),
            bounds.len(),
            bounds.len() + 1
        ));
    }
    let total = buckets.iter().fold(0u64, |sum, bucket| sum.saturating_add(*bucket));
    if total != count {
        return Err(format!("{}:{line}: invalid histogram `{name}`: bucketCounts sum {total} != count {count}", path.display()));
    }
    for pair in bounds.windows(2) {
        if pair[0] >= pair[1] {
            return Err(format!("{}:{line}: invalid histogram `{name}`: explicitBounds are not strictly increasing", path.display()));
        }
    }
    Ok(())
}

/// The latest cumulative snapshot per series, or the sum of the delta snapshots.
fn resolved(run: &Run, name: &str) -> Vec<MetricPoint> {
    let mut groups: BTreeMap<String, Vec<&MetricPoint>> = BTreeMap::new();
    for point in run.metrics.iter().filter(|point| point.name == name) {
        groups.entry(point.key()).or_default().push(point);
    }
    let mut out = Vec::with_capacity(groups.len());
    for group in groups.values() {
        let Some(first) = group.first() else {
            continue;
        };
        let mut point = (*first).clone();
        if group.iter().any(|sample| sample.delta) {
            for sample in group.iter().skip(1) {
                point.merge_delta(sample);
            }
        } else if let Some(last) = group.iter().max_by_key(|sample| sample.time_ns) {
            point = (*last).clone();
        }
        out.push(point);
    }
    out
}

fn sum_of(run: &Run, name: &str) -> Option<f64> {
    let points = resolved(run, name);
    if points.is_empty() {
        return None;
    }
    Some(
        points
            .iter()
            .filter_map(|point| match point.value {
                MetricValue::Sum(value) => Some(value),
                _ => None,
            })
            .sum(),
    )
}

fn hist_totals(run: &Run, name: &str) -> Option<(u64, f64, Vec<u64>, Vec<f64>)> {
    let points = resolved(run, name);
    let mut count = 0u64;
    let mut sum = 0.0;
    let mut buckets: Vec<u64> = Vec::new();
    let mut bounds: Vec<f64> = Vec::new();
    let mut seen = false;
    for point in &points {
        if let MetricValue::Hist { count: c, sum: s, buckets: b, bounds: e } = &point.value {
            seen = true;
            count = count.saturating_add(*c);
            sum += s;
            if buckets.is_empty() {
                buckets = b.clone();
                bounds = e.clone();
            } else {
                for (left, right) in buckets.iter_mut().zip(b) {
                    *left = left.saturating_add(*right);
                }
            }
        }
    }
    seen.then_some((count, sum, buckets, bounds))
}

fn mean_of_histogram(run: &Run, name: &str) -> Option<f64> {
    let (count, sum, _, _) = hist_totals(run, name)?;
    (count > 0).then(|| sum / count as f64)
}

fn gauge_last(run: &Run, name: &str) -> Option<f64> {
    resolved(run, name)
        .iter()
        .filter_map(|point| match point.value {
            MetricValue::Gauge(value) => Some((point.time_ns, value)),
            _ => None,
        })
        .max_by_key(|(time, _)| *time)
        .map(|(_, value)| value)
}

/// Standard histogram quantile: linear interpolation inside the bucket that contains the rank.
/// Unlike a span-derived percentile this is an **estimate**; the report says so in the runbook.
/// The final bucket is `(+last_bound, +Inf)`: a rank that lands there is larger than anything we
/// can interpolate, so the honest answer is `None` (`n/a`), never the last explicit bound.
fn histogram_quantile(bounds: &[f64], buckets: &[u64], count: u64, quantile: f64) -> Option<f64> {
    if count == 0 || buckets.is_empty() {
        return None;
    }
    let rank = (quantile * count as f64).max(1.0);
    let mut cumulative = 0u64;
    for (index, bucket) in buckets.iter().enumerate() {
        let previous = cumulative;
        cumulative = cumulative.saturating_add(*bucket);
        if (cumulative as f64) < rank {
            continue;
        }
        // index == bounds.len() is the +Inf bucket.
        if index >= bounds.len() {
            return None;
        }
        let lower = if index == 0 { 0.0 } else { bounds[index - 1] };
        let upper = bounds[index];
        if *bucket == 0 {
            return Some(lower);
        }
        let fraction = ((rank - previous as f64) / *bucket as f64).clamp(0.0, 1.0);
        return Some(lower + (upper - lower) * fraction);
    }
    None
}

#[derive(Clone, Copy, Debug, Default)]
struct Stats {
    p50: f64,
    p90: f64,
    p99: f64,
    worst: f64,
    cv: f64,
    cv_median: f64,
    one_percent_low_fps: f64,
}

fn percentile_sorted(sorted: &[f64], quantile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((quantile * sorted.len() as f64).ceil() as usize).max(1).min(sorted.len());
    sorted[rank - 1]
}

fn median_sorted(sorted: &[f64]) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) { (sorted[middle - 1] + sorted[middle]) / 2.0 } else { sorted[middle] }
}

/// Relative standard error of the median, estimated by a deterministic bootstrap.
///
/// §8.3's "CV > 3% for a median" is a run-to-run quantity, but one directory holds one run: the
/// honest single-run analogue is how much the median would move under resampling. The fixed seed
/// keeps the report reproducible.
fn bootstrap_cv_median(sorted: &[f64]) -> f64 {
    if sorted.len() < 2 {
        return 0.0;
    }
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut buffer = Vec::with_capacity(sorted.len());
    let mut medians = Vec::with_capacity(BOOTSTRAP_RESAMPLES);
    for _ in 0..BOOTSTRAP_RESAMPLES {
        buffer.clear();
        for _ in 0..sorted.len() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            buffer.push(sorted[(state as usize) % sorted.len()]);
        }
        buffer.sort_by(f64::total_cmp);
        medians.push(median_sorted(&buffer));
    }
    let mean = medians.iter().sum::<f64>() / medians.len() as f64;
    if mean <= 0.0 {
        return 0.0;
    }
    let variance = medians.iter().map(|median| (median - mean).powi(2)).sum::<f64>() / medians.len() as f64;
    variance.sqrt() / mean
}

fn stats(values: &[f64]) -> Option<Stats> {
    let mut sorted: Vec<f64> = values.iter().copied().filter(|value| value.is_finite()).collect();
    if sorted.is_empty() {
        return None;
    }
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    let mean = sorted.iter().sum::<f64>() / n as f64;
    let variance = sorted.iter().map(|value| (value - mean).powi(2)).sum::<f64>() / n as f64;
    let deviation = variance.sqrt();
    let slow = ((n as f64 * 0.01).ceil() as usize).max(1).min(n);
    let slowest_mean = sorted[n - slow..].iter().sum::<f64>() / slow as f64;
    Some(Stats {
        p50: percentile_sorted(&sorted, 0.50),
        p90: percentile_sorted(&sorted, 0.90),
        p99: percentile_sorted(&sorted, 0.99),
        worst: sorted[n - 1],
        cv: if mean > 0.0 { deviation / mean } else { 0.0 },
        cv_median: bootstrap_cv_median(&sorted),
        one_percent_low_fps: if slowest_mean > 0.0 { 1000.0 / slowest_mean } else { 0.0 },
    })
}

#[derive(Clone, Debug, Default)]
struct Jank {
    missed_vsync: u64,
    dropped_frame: u64,
    long_task: u64,
    episodes: u64,
    longest_run: u64,
}

/// The same exclusive classes `crates/ui-egui/src/otel_pacing.rs` assigns, from the same constants.
fn jank_class(frame_ms: f64) -> Option<&'static str> {
    if !frame_ms.is_finite() {
        return None;
    }
    if frame_ms > sc::LONG_TASK_MS {
        Some(sc::V_JANK_LONG_TASK)
    } else if frame_ms > 2.0 * sc::FRAME_BUDGET_MS {
        Some(sc::V_JANK_DROPPED_FRAME)
    } else if frame_ms > sc::FRAME_BUDGET_MS {
        Some(sc::V_JANK_MISSED_VSYNC)
    } else {
        None
    }
}

#[derive(Clone, Debug, Default)]
struct Cache {
    hit: u64,
    miss: u64,
    thrash: u64,
}

#[derive(Clone, Debug, Default)]
struct Inp {
    kind: String,
    total: Option<Stats>,
    input_delay: Option<f64>,
    processing: Option<f64>,
    presentation: Option<f64>,
    coalesced: u64,
}

#[derive(Clone, Debug)]
struct Phase {
    name: String,
    p99: f64,
    share: f64,
}

#[derive(Clone, Debug, Default)]
struct Raster {
    ms_per_mpx: Option<f64>,
    mpx_per_s: Option<f64>,
    average_mpx: Option<f64>,
    gb_per_s: Option<f64>,
}

#[derive(Clone, Debug, Default)]
struct Gpu {
    frame_ms: Option<f64>,
    utilization: Option<f64>,
    memory_mib: Option<f64>,
}

#[derive(Clone, Debug, Default)]
struct Pacing {
    late: u64,
    duplicate: u64,
    split: u64,
    error_p99: Option<f64>,
}

#[derive(Clone, Debug, Default)]
struct Health {
    dropped_records: u64,
    export_p99: Option<f64>,
    log_errors: usize,
}

#[derive(Clone, Debug, Default)]
struct Summary {
    frames: usize,
    stats: Option<Stats>,
    jank: Jank,
    cache: Cache,
    hit_rate: Option<f64>,
    inp: Inp,
    phases: Vec<Phase>,
    raster: Raster,
    gpu: Gpu,
    pacing: Pacing,
    health: Health,
    renders_per_interaction: Option<f64>,
    idle_repaint_rate: Option<f64>,
    zoom: f64,
}

impl Summary {
    fn build(run: &Run) -> Summary {
        let mut frames: Vec<&Span> = run.spans.iter().filter(|span| span.name == sc::S_FRAME).collect();
        frames.sort_by_key(|span| span.start_ns);
        let durations: Vec<f64> = frames.iter().map(|span| duration_ms(span)).collect();
        let stats = stats(&durations);
        let jank = jank(run, &durations);
        let cache = cache(run);
        let hit_rate = (cache.hit + cache.miss > 0).then(|| cache.hit as f64 / (cache.hit + cache.miss) as f64);
        let inp = inp(run);
        let phases = phases(run);
        let raster = raster(run);
        let gpu = gpu(run);
        let pacing = pacing(run);
        let health = health(run, frames.len());
        let renders_per_interaction = match (hist_totals(run, sc::M_RASTER_DURATION), sum_of(run, sc::M_INTERACTION_COUNT)) {
            (Some((count, _, _, _)), Some(interactions)) if interactions > 0.0 => Some(count as f64 / interactions),
            _ => None,
        };
        let idle_repaint_rate = idle_repaint_rate(run, &frames);
        let zoom = run.resource.get(sc::A_TEST_ZOOM).and_then(Attr::as_f64).unwrap_or(1.0);
        Summary {
            frames: frames.len(),
            stats,
            jank,
            cache,
            hit_rate,
            inp,
            phases,
            raster,
            gpu,
            pacing,
            health,
            renders_per_interaction,
            idle_repaint_rate,
            zoom,
        }
    }
}

fn jank(run: &Run, durations: &[f64]) -> Jank {
    let mut jank = Jank::default();
    let points = resolved(run, sc::M_FRAME_JANKY);
    if points.is_empty() {
        for duration in durations {
            match jank_class(*duration) {
                Some(sc::V_JANK_MISSED_VSYNC) => jank.missed_vsync += 1,
                Some(sc::V_JANK_DROPPED_FRAME) => jank.dropped_frame += 1,
                Some(sc::V_JANK_LONG_TASK) => jank.long_task += 1,
                _ => {}
            }
        }
    } else {
        for point in &points {
            let MetricValue::Sum(value) = point.value else {
                continue;
            };
            match point.attrs.get(sc::A_JANK_CLASS).and_then(Attr::as_str) {
                Some(sc::V_JANK_MISSED_VSYNC) => jank.missed_vsync = jank.missed_vsync.saturating_add(value as u64),
                Some(sc::V_JANK_DROPPED_FRAME) => jank.dropped_frame = jank.dropped_frame.saturating_add(value as u64),
                Some(sc::V_JANK_LONG_TASK) => jank.long_task = jank.long_task.saturating_add(value as u64),
                _ => {}
            }
        }
    }
    let (episodes, longest_run) = episodes(durations);
    jank.episodes = episodes;
    jank.longest_run = longest_run;
    jank
}

/// Maximal consecutive runs of frames over the 60 Hz budget: `(episodes, longest)`.
fn episodes(durations: &[f64]) -> (u64, u64) {
    let mut episodes = 0u64;
    let mut longest = 0u64;
    let mut run = 0u64;
    for duration in durations {
        if *duration > sc::FRAME_BUDGET_MS {
            run = run.saturating_add(1);
            if run == 1 {
                episodes = episodes.saturating_add(1);
            }
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    (episodes, longest)
}

fn cache(run: &Run) -> Cache {
    let mut cache = Cache::default();
    for point in resolved(run, sc::M_CACHE) {
        let MetricValue::Sum(value) = point.value else {
            continue;
        };
        match point.attrs.get(sc::A_CACHE_RESULT).and_then(Attr::as_str) {
            Some(sc::V_HIT) => cache.hit = cache.hit.saturating_add(value as u64),
            Some(sc::V_MISS) => cache.miss = cache.miss.saturating_add(value as u64),
            _ => {}
        }
    }
    cache.thrash = sum_of(run, sc::M_CACHE_THRASH).unwrap_or(0.0) as u64;
    cache
}

fn inp(run: &Run) -> Inp {
    let mut groups: BTreeMap<String, Vec<&Span>> = BTreeMap::new();
    for span in run.spans.iter().filter(|span| span.name == sc::S_INTERACTION) {
        let kind = span.attrs.get(sc::A_INTERACTION_TYPE).and_then(Attr::as_str).unwrap_or("interaction").to_owned();
        groups.entry(kind).or_default().push(span);
    }
    let Some((kind, spans)) = groups.iter().max_by_key(|(_, spans)| spans.len()) else {
        return Inp { coalesced: sum_of(run, sc::M_INTERACTION_COALESCED).unwrap_or(0.0) as u64, ..Inp::default() };
    };
    let totals: Vec<f64> = spans.iter().filter_map(|span| span.attrs.get(sc::A_INP_TOTAL_MS).and_then(Attr::as_f64)).collect();
    let phase = |key: &str| {
        let values: Vec<f64> = spans.iter().filter_map(|span| span.attrs.get(key).and_then(Attr::as_f64)).collect();
        stats(&values).map(|stats| stats.p50)
    };
    Inp {
        kind: kind.clone(),
        total: stats(&totals),
        input_delay: phase(sc::A_INP_INPUT_DELAY_MS),
        processing: phase(sc::A_INP_PROCESSING_MS),
        presentation: phase(sc::A_INP_PRESENTATION_MS),
        coalesced: sum_of(run, sc::M_INTERACTION_COALESCED).unwrap_or(0.0) as u64,
    }
}

fn phase_of(span: &Span) -> Option<&str> {
    span.attrs.get(sc::A_PHASE).and_then(Attr::as_str).or_else(|| span.name.strip_prefix(sc::S_PHASE_PREFIX))
}

/// The phase table: one histogram, one `phase` attribute. Raster/convert/upload exist only as
/// instruments (they have no span), so the table reads the aggregated histogram; the frame
/// percentiles above stay exact because they come from spans.
fn phases(run: &Run) -> Vec<Phase> {
    let mut groups: BTreeMap<String, (u64, Vec<u64>, Vec<f64>)> = BTreeMap::new();
    for point in resolved(run, sc::M_FRAME_PHASE) {
        let MetricValue::Hist { count, buckets, bounds, .. } = &point.value else {
            continue;
        };
        let Some(name) = point.attrs.get(sc::A_PHASE).and_then(Attr::as_str) else {
            continue;
        };
        let entry = groups.entry(name.to_owned()).or_insert_with(|| (0, Vec::new(), bounds.clone()));
        entry.0 = entry.0.saturating_add(*count);
        if entry.1.is_empty() {
            entry.1 = buckets.clone();
        } else {
            for (left, right) in entry.1.iter_mut().zip(buckets) {
                *left = left.saturating_add(*right);
            }
        }
    }
    let mut out: Vec<Phase> = groups
        .into_iter()
        .filter_map(|(name, (count, buckets, bounds))| histogram_quantile(&bounds, &buckets, count, 0.99).map(|p99| Phase { name, p99, share: 0.0 }))
        .collect();
    if out.is_empty() {
        let mut spans: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        for span in &run.spans {
            if let Some(name) = phase_of(span) {
                spans.entry(name.to_owned()).or_default().push(duration_ms(span));
            }
        }
        out = spans.into_iter().filter_map(|(name, values)| stats(&values).map(|stats| Phase { name, p99: stats.p99, share: 0.0 })).collect();
    }
    let total: f64 = out.iter().map(|phase| phase.p99).sum();
    for phase in &mut out {
        phase.share = if total > 0.0 { phase.p99 / total * 100.0 } else { 0.0 };
    }
    out.sort_by(|left, right| right.p99.total_cmp(&left.p99));
    out
}

/// Mínimo de duração somada de uma fase para derivar uma taxa. No harness headless o `tex.set` é
/// quase um no-op: dividir bytes reais por ~0 ms produz terabytes por segundo.
const MIN_PHASE_MS_FOR_RATE: f64 = 1.0;

/// Nenhum caminho de cópia host→device de uma estação de trabalho passa disto (PCIe 5.0 x16 fica
/// em ~60 GB/s; memória unificada copia bem abaixo de 200 GB/s). Uma taxa derivada acima do teto
/// não mede um dispositivo rápido: mede um denominador que não contém o trabalho — no harness
/// headless `texture.bytes` conta pixels reais enquanto `tex.set` leva ~3 µs por quadro.
const MAX_PLAUSIBLE_GB_PER_S: f64 = 256.0;

/// GB/s from a summed byte count and a summed phase duration (both already in `ms` / bytes).
/// `None` when the pair is not a measurement: a denominator too small to mean anything, or an
/// implied rate above the bus ceiling. The report prints `n/a` instead of a number that would
/// blame the wrong device.
fn bytes_per_second(byte_sum: f64, phase_ms: f64) -> Option<f64> {
    if !byte_sum.is_finite() || !phase_ms.is_finite() || byte_sum <= 0.0 || phase_ms < MIN_PHASE_MS_FOR_RATE {
        return None;
    }
    let gb_per_s = byte_sum / (phase_ms / 1000.0) / 1.0e9;
    (gb_per_s <= MAX_PLAUSIBLE_GB_PER_S).then_some(gb_per_s)
}

fn raster(run: &Run) -> Raster {
    let duration = hist_totals(run, sc::M_RASTER_DURATION);
    let pixels = hist_totals(run, sc::M_RASTER_PIXELS);
    let bytes = hist_totals(run, sc::M_TEXTURE_BYTES);
    let upload_ms = phase_sum(run, sc::P_UPLOAD);
    let (Some((_, raster_ms, _, _)), Some((pixel_count, raster_px, _, _))) = (duration, pixels) else {
        return Raster::default();
    };
    let ms_per_mpx = (raster_px > 0.0).then(|| raster_ms / raster_px);
    let mpx_per_s = (raster_ms > 0.0).then(|| raster_px / (raster_ms / 1000.0));
    let average_mpx = (pixel_count > 0).then(|| raster_px / pixel_count as f64);
    let gb_per_s = match (bytes, upload_ms) {
        (Some((_, byte_sum, _, _)), Some(upload_ms)) => bytes_per_second(byte_sum, upload_ms),
        _ => None,
    };
    Raster { ms_per_mpx, mpx_per_s, average_mpx, gb_per_s }
}

/// The summed duration of one `frame.phase.duration` series, in ms.
fn phase_sum(run: &Run, phase: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut seen = false;
    for point in resolved(run, sc::M_FRAME_PHASE) {
        if point.attrs.get(sc::A_PHASE).and_then(Attr::as_str) != Some(phase) {
            continue;
        }
        if let MetricValue::Hist { sum, .. } = point.value {
            total += sum;
            seen = true;
        }
    }
    seen.then_some(total)
}

fn gpu(run: &Run) -> Gpu {
    Gpu {
        frame_ms: mean_of_histogram(run, sc::M_GPU_FRAME_TIME),
        utilization: gauge_last(run, sc::M_GPU_UTILIZATION),
        memory_mib: gauge_last(run, sc::M_GPU_MEMORY_USED).map(|bytes| bytes / 1_048_576.0),
    }
}

fn pacing(run: &Run) -> Pacing {
    let error_p99 = hist_totals(run, sc::M_PACING_ERROR).and_then(|(count, _, buckets, bounds)| histogram_quantile(&bounds, &buckets, count, 0.99));
    let mut pacing = Pacing {
        late: sum_of(run, sc::M_FRAME_LATE).unwrap_or(0.0) as u64,
        duplicate: sum_of(run, sc::M_FRAME_DUPLICATE).unwrap_or(0.0) as u64,
        split: sum_of(run, sc::M_FRAME_SPLIT).unwrap_or(0.0) as u64,
        error_p99,
    };
    // A run exported without metrics still carries the same verdicts as span events; count them
    // rather than reporting the absence of a counter as zero proxies.
    if pacing.late == 0 && pacing.duplicate == 0 && pacing.split == 0 {
        for span in &run.spans {
            for event in &span.events {
                if event.name != sc::E_TEARING_SUSPECT {
                    continue;
                }
                match event.attrs.get(sc::A_TEARING_PROXY).and_then(Attr::as_str) {
                    Some(sc::V_TEARING_LATE) => pacing.late = pacing.late.saturating_add(1),
                    Some(sc::V_TEARING_DUPLICATE) => pacing.duplicate = pacing.duplicate.saturating_add(1),
                    Some(sc::V_TEARING_SPLIT) => pacing.split = pacing.split.saturating_add(1),
                    _ => {}
                }
            }
        }
    }
    pacing
}

/// Compare the painted-frame count with the `frame.duration` histogram's sample count.
///
/// `deckcraft.telemetry.dropped_records` is deliberately never emitted: SDK 0.33.1 exposes no
/// batch-processor drop counter, and a fabricated zero would be the one number the report depends
/// on being true. A shortfall between the two independent counts is the honest signal.
fn health(run: &Run, frame_spans: usize) -> Health {
    let painted = hist_totals(run, sc::M_FRAME_DURATION).map(|(count, _, _, _)| count).unwrap_or(frame_spans as u64);
    let dropped_records = painted.saturating_sub(frame_spans as u64);
    let export_p99 =
        hist_totals(run, sc::M_EXPORT_DURATION).and_then(|(count, _, buckets, bounds)| histogram_quantile(&bounds, &buckets, count, 0.99));
    Health { dropped_records, export_p99, log_errors: run.log_errors }
}

fn idle_repaint_rate(run: &Run, frames: &[&Span]) -> Option<f64> {
    let points = resolved(run, sc::M_REPAINT_REQUESTED);
    if points.is_empty() {
        return None;
    }
    let continuous: f64 = points
        .iter()
        .filter(|point| point.attrs.get(sc::A_REPAINT_SOURCE).and_then(Attr::as_str) == Some(sc::V_REPAINT_CONTINUOUS))
        .filter_map(|point| match point.value {
            MetricValue::Sum(value) => Some(value),
            _ => None,
        })
        .sum();
    let first = frames.first()?.start_ns;
    let last = frames.last()?.end_ns;
    let seconds = last.saturating_sub(first) as f64 / 1.0e9;
    (seconds > 0.0).then(|| continuous / seconds)
}

fn ratio(candidate: Option<f64>, baseline: Option<f64>) -> Option<f64> {
    match (candidate, baseline) {
        (Some(candidate), Some(baseline)) if baseline > 0.0 && candidate.is_finite() => Some(candidate / baseline),
        _ => None,
    }
}

fn resource_mismatch(candidate: &Run, baseline: &Run) -> Option<String> {
    for key in COMPARABLE_RESOURCE_KEYS {
        let left = candidate.resource.get(*key).map(Attr::display);
        let right = baseline.resource.get(*key).map(Attr::display);
        if left != right {
            return Some(format!(
                "{key}: candidate={} baseline={}",
                left.unwrap_or_else(|| "absent".into()),
                right.unwrap_or_else(|| "absent".into())
            ));
        }
    }
    None
}

fn format_ms(value: Option<f64>) -> String {
    value.map(|value| format!("{value:.2}ms")).unwrap_or_else(|| "n/a".into())
}

fn format_ratio(value: Option<f64>) -> String {
    value.map(|value| format!("{value:.2}x")).unwrap_or_else(|| "n/a".into())
}

fn format_cv(value: Option<f64>) -> String {
    value.map(|value| format!("{value:.3}")).unwrap_or_else(|| "n/a".into())
}

fn print_report(candidate: &Summary, baseline: Option<&Summary>, run: &Run) -> Result<(), String> {
    let service = run.resource.get(sc::SERVICE_NAME).and_then(Attr::as_str).unwrap_or("unknown");
    let version = run.resource.get(sc::SERVICE_VERSION).and_then(Attr::as_str).unwrap_or("unknown");
    let variant = run.resource.get(sc::A_TEST_VARIANT).and_then(Attr::as_str).unwrap_or("unknown");
    let run_id = run.resource.get(sc::A_TEST_RUN_ID).and_then(Attr::as_str).unwrap_or("unknown");
    println!("DeckCraft OTLP report   service={service} {version}  variant={variant}  run={run_id}");

    let stats = candidate.stats;
    println!(
        "frames: {}   p50={} p90={} p99={} worst={}  1%low_fps={}  CV={}",
        candidate.frames,
        format_ms(stats.map(|stats| stats.p50)),
        format_ms(stats.map(|stats| stats.p90)),
        format_ms(stats.map(|stats| stats.p99)),
        format_ms(stats.map(|stats| stats.worst)),
        stats.map(|stats| format!("{:.1}", stats.one_percent_low_fps)).unwrap_or_else(|| "n/a".into()),
        format_cv(stats.map(|stats| stats.cv)),
    );
    println!(
        "jank:   missed_vsync={} dropped_frame={} long_task={}  jank={} longest_run={}",
        candidate.jank.missed_vsync, candidate.jank.dropped_frame, candidate.jank.long_task, candidate.jank.episodes, candidate.jank.longest_run
    );
    println!(
        "cache:  hit={} miss={}  hit_rate={}  renders/interaction={}  thrash={}",
        candidate.cache.hit,
        candidate.cache.miss,
        candidate.hit_rate.map(|rate| format!("{:.1}%", rate * 100.0)).unwrap_or_else(|| "n/a".into()),
        candidate.renders_per_interaction.map(|ratio| format!("{ratio:.2}")).unwrap_or_else(|| "n/a".into()),
        candidate.cache.thrash
    );
    let inp = &candidate.inp;
    println!(
        "INP ({}): p50={} p90={} p99={} worst={}",
        if inp.kind.is_empty() { "none" } else { &inp.kind },
        format_ms(inp.total.map(|stats| stats.p50)),
        format_ms(inp.total.map(|stats| stats.p90)),
        format_ms(inp.total.map(|stats| stats.p99)),
        format_ms(inp.total.map(|stats| stats.worst)),
    );
    println!(
        "  input_delay={}  processing={}  presentation={}  coalesced={}",
        format_ms(inp.input_delay),
        format_ms(inp.processing),
        format_ms(inp.presentation),
        inp.coalesced
    );

    println!("bottleneck (p99 by phase):        cpu/wall");
    if candidate.phases.is_empty() {
        println!("  (no phase samples)");
    } else {
        for phase in &candidate.phases {
            println!("  {:<12} {:>6.2} ms  {:>5.1}%  {:>8}", phase.name, phase.p99, phase.share, "n/a");
        }
    }
    println!(
        "raster efficiency: {}  (zoom {:.1}, {} Mpx, {})   upload {}",
        candidate.raster.ms_per_mpx.map(|value| format!("{value:.2} ms/Mpx")).unwrap_or_else(|| "n/a".into()),
        candidate.zoom,
        candidate.raster.average_mpx.map(|value| format!("{value:.2}")).unwrap_or_else(|| "n/a".into()),
        candidate.raster.mpx_per_s.map(|value| format!("{value:.1} Mpx/s")).unwrap_or_else(|| "n/a".into()),
        candidate.raster.gb_per_s.map(|value| format!("{value:.2} GB/s")).unwrap_or_else(|| "n/a".into()),
    );
    println!(
        "gpu: frame {}  util {}  mem {}     repaints/s idle {}",
        format_ms(candidate.gpu.frame_ms),
        candidate.gpu.utilization.map(|value| format!("{value:.2}")).unwrap_or_else(|| "n/a".into()),
        candidate.gpu.memory_mib.map(|value| format!("{value:.0} MiB")).unwrap_or_else(|| "n/a".into()),
        candidate.idle_repaint_rate.map(|value| format!("{value:.1}")).unwrap_or_else(|| "n/a".into()),
    );
    println!(
        "tearing proxies: late={} duplicate={} split={} pacing_error_p99={}",
        candidate.pacing.late,
        candidate.pacing.duplicate,
        candidate.pacing.split,
        format_ms(candidate.pacing.error_p99)
    );
    println!(
        "telemetry health: dropped_records={}  export_p99={}{}",
        candidate.health.dropped_records,
        format_ms(candidate.health.export_p99),
        if candidate.health.log_errors > 0 { format!("  log_errors={}", candidate.health.log_errors) } else { String::new() }
    );

    let Some(baseline) = baseline else {
        return finish_gates(candidate, None, "PASS: no baseline, gates not applied");
    };
    if candidate.stats.is_none() || baseline.stats.is_none() {
        println!("A/B (candidate/baseline): n/a (no frames in candidate or baseline)");
        return finish_gates(candidate, None, "PASS: no comparable baseline");
    }
    let p50 = ratio(stats.map(|stats| stats.p50), baseline.stats.map(|stats| stats.p50));
    let p99 = ratio(stats.map(|stats| stats.p99), baseline.stats.map(|stats| stats.p99));
    let inp_ratio = ratio(candidate.inp.total.map(|stats| stats.p99), baseline.inp.total.map(|stats| stats.p99));
    let renders = ratio(candidate.renders_per_interaction, baseline.renders_per_interaction);
    println!(
        "A/B (candidate/baseline): p50 {}  p99 {}  INP {}  renders/interaction {}",
        format_ratio(p50),
        format_ratio(p99),
        format_ratio(inp_ratio),
        format_ratio(renders)
    );
    finish_gates(candidate, Some((p50, p99)), "PASS: p99 within gate (<=1.10x), median within gate (<=1.05x)")
}

fn finish_gates(candidate: &Summary, comparison: Option<(Option<f64>, Option<f64>)>, pass_message: &str) -> Result<(), String> {
    let mut failures: Vec<String> = Vec::new();
    if candidate.health.dropped_records > 0 {
        failures.push(format!("dropped_records={} (the telemetry pipeline lost records; the run is invalid)", candidate.health.dropped_records));
    }
    if candidate.frames >= MIN_CV_FRAMES
        && let Some(stats) = candidate.stats
        && stats.cv_median > CV_GATE
    {
        failures.push(format!("median CV {:.1}% > {:.0}% — rerun on a quieter host", stats.cv_median * 100.0, CV_GATE * 100.0));
    }
    if let Some((p50, p99)) = comparison {
        if let Some(p99) = p99
            && p99 > P99_GATE
        {
            failures.push(format!("p99 {p99:.2}x > {P99_GATE:.2}x"));
        }
        if let Some(p50) = p50
            && p50 > P50_GATE
        {
            failures.push(format!("median {p50:.2}x > {P50_GATE:.2}x"));
        }
    }
    if failures.is_empty() {
        println!("{pass_message}");
        return Ok(());
    }
    for failure in &failures {
        println!("FAIL: {failure}");
    }
    Err(format!("{} gate(s) failed", failures.len()))
}

/// Parse the CLI arguments: `<dir> [--baseline <dir>]`.
fn parse_args(root: &Path, args: &[&str]) -> Result<(PathBuf, Option<PathBuf>), String> {
    let mut dir: Option<PathBuf> = None;
    let mut baseline: Option<PathBuf> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index] {
            "--baseline" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err("--baseline needs a directory".into());
                };
                baseline = Some(resolve(root, value));
            }
            flag if flag.starts_with("--") => return Err(format!("unknown flag `{flag}`")),
            value => {
                if dir.is_some() {
                    return Err(format!("unexpected argument `{value}` (one directory only)"));
                }
                dir = Some(resolve(root, value));
            }
        }
        index += 1;
    }
    let Some(dir) = dir else {
        return Err("usage: cargo xtask otel-report <dir> [--baseline <dir>]".into());
    };
    Ok((dir, baseline))
}

fn resolve(root: &Path, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() { path.to_path_buf() } else { root.join(path) }
}

/// `cargo xtask otel-report <dir> [--baseline <dir>]`.
pub fn run(root: &Path, args: &[&str]) -> Result<(), String> {
    let (dir, baseline_dir) = parse_args(root, args)?;
    let run = Run::load(&dir)?;
    for note in &run.notes {
        eprintln!("note: {note}");
    }
    let candidate = Summary::build(&run);
    let baseline = match baseline_dir {
        Some(baseline_dir) => {
            let baseline_run = Run::load(&baseline_dir)?;
            for note in &baseline_run.notes {
                eprintln!("note: {note}");
            }
            let baseline_summary = Summary::build(&baseline_run);
            // A run with no frames has no resource identity to compare; the report says "no
            // comparable baseline" instead of pretending the missing adapter is a mismatch.
            if candidate.frames > 0
                && baseline_summary.frames > 0
                && let Some(mismatch) = resource_mismatch(&run, &baseline_run)
            {
                println!("REFUSED: resource mismatch ({mismatch})");
                return Err("refusing to compare runs from different environments".into());
            }
            Some(baseline_summary)
        }
        None => None,
    };
    print_report(&candidate, baseline.as_ref(), &run)
}

#[cfg(test)]
mod quantile_tests {
    use super::histogram_quantile;

    #[test]
    fn an_overflow_bucket_is_not_reported_as_the_last_explicit_bound() {
        let bounds = [4.0, 8.0, 16.7, 1000.0];
        // 9 samples inside the bounds, 1 in the (+1000, +Inf) bucket.
        let buckets = [1_u64, 2, 3, 3, 1];
        let p50 = histogram_quantile(&bounds, &buckets, 10, 0.50).expect("p50 inside bounds");
        assert!((p50 - 13.8).abs() < 0.2, "p50 interpolado: {p50}");
        assert_eq!(histogram_quantile(&bounds, &buckets, 10, 0.99), None, "p99 cai no +Inf");
    }

    #[test]
    fn a_normal_bucket_still_interpolates() {
        let bounds = [4.0, 8.0, 16.7];
        let buckets = [1_u64, 2, 3, 0];
        assert!(histogram_quantile(&bounds, &buckets, 6, 0.50).is_some());
        assert_eq!(histogram_quantile(&bounds, &buckets, 0, 0.50), None);
    }
}

#[cfg(test)]
mod rate_tests {
    use super::bytes_per_second;

    #[test]
    fn a_no_op_upload_cannot_produce_a_throughput() {
        // 13,3 GB reais medidos contra ~0,003 ms de tex.set no harness headless.
        assert_eq!(bytes_per_second(13.3e9, 0.003), None);
        assert_eq!(bytes_per_second(13.3e9, 0.0), None);
        assert_eq!(bytes_per_second(f64::NAN, 3300.0), None);
    }

    #[test]
    fn a_real_upload_produces_a_plausible_gb_per_s() {
        let rate = bytes_per_second(13.3e9, 3300.0).expect("13,3 GB em 3,3 s");
        assert!((rate - 4.03).abs() < 0.1, "got {rate}");
        assert!(rate < 32.0, "acima de PCIe é mentira: {rate}");
    }

    /// Captura real (`target/otel/candidate`): 13,31 GB de bytes de textura contra 3,057 ms
    /// somados em 1003 quadros do harness headless, onde `tex.set` é no-op. O total é 3 µs por
    /// quadro — mil vezes maior que a estimativa de 0,003 ms, então um piso de 1 ms não o pega.
    /// O que denuncia o absurdo é a própria taxa: 4,3 TB/s não existe.
    #[test]
    fn a_rate_above_the_bus_ceiling_is_not_a_measurement() {
        assert_eq!(bytes_per_second(13.31e9, 3.057), None, "4,3 TB/s é impossível");
    }
}
