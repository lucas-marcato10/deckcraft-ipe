//! What a frame and its texture pipeline actually emit into OpenTelemetry.
//!
//! The crate carries no SDK (it is L7 over the OTel *API*); the app installs the provider at its
//! own boundary. These tests install the same `InMemoryMetricExporter` the SDK ships for exactly
//! this purpose and read back what landed — a metric name that never appears means the call site
//! never recorded, which no compiler and no unit test of the handles can catch.
//!
//! The meter provider is process-global and can only be installed once, so [`pipeline`] installs
//! it for the whole test binary. `SlideApp::new` calls it before the first frame (see the
//! `#[cfg(test)]` line there): OTel binds an instrument to the provider that is global when the
//! handle is built, so installing it after any test had painted would leave the handles no-op.
//! The tests that read the exporter serialise on [`reader`] so one cannot reset/collect while the
//! other is mid-frame.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Mutex, MutexGuard, OnceLock};

use deckcraft_engine::Session;
use deckcraft_telemetry::sc;
use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};

use crate::{Services, SlideApp};

/// The in-memory metric pipeline every test in this binary records into.
pub(crate) struct Pipeline {
    exporter: InMemoryMetricExporter,
    provider: SdkMeterProvider,
}

static PIPELINE: OnceLock<Pipeline> = OnceLock::new();

/// Install the in-memory meter provider (once) and return it.
///
/// `init_instruments()` runs right after the provider is installed: the handle set binds to the
/// global meter at first use, and a handle built against the default no-op provider would stay
/// no-op for the rest of the process.
pub(crate) fn pipeline() -> &'static Pipeline {
    PIPELINE.get_or_init(|| {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder().with_periodic_exporter(exporter.clone()).build();
        opentelemetry::global::set_meter_provider(provider.clone());
        deckcraft_telemetry::init_instruments();
        Pipeline { exporter, provider }
    })
}

/// Serialises the tests that read the exporter.
static READER: Mutex<()> = Mutex::new(());

fn reader() -> MutexGuard<'static, ()> {
    // A panicking test poisons the mutex, not the pipeline: the next reader still has to run.
    READER.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One data point the exporter reports: the metric name, its attributes, and its value.
///
/// For a histogram the value is its `count` (the number of times it was recorded), for a sum its
/// accumulated value — enough to tell "recorded at all" from "recorded once more".
#[derive(Debug)]
struct Point {
    metric: String,
    attrs: Vec<(String, String)>,
    value: f64,
}

impl Point {
    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str())
    }
}

fn push<'a>(out: &mut Vec<Point>, metric: &str, attrs: impl Iterator<Item = &'a KeyValue>, value: f64) {
    out.push(Point { metric: metric.to_owned(), attrs: attrs.map(|kv| (kv.key.to_string(), kv.value.to_string())).collect(), value });
}

/// Every data point the in-memory exporter holds after a forced collection.
///
/// Batches accumulate (the exporter never drains on read), so values only grow and a
/// before/after difference is a sound "this recorded more" check even if another test shares the
/// same process.
fn points() -> Vec<Point> {
    let pipeline = pipeline();
    pipeline.provider.force_flush().unwrap();
    let batches = pipeline.exporter.get_finished_metrics().unwrap();
    let mut out = Vec::new();
    for batch in &batches {
        for scope in batch.scope_metrics() {
            for metric in scope.metrics() {
                let name = metric.name();
                match metric.data() {
                    AggregatedMetrics::F64(data) => match data {
                        MetricData::Histogram(hist) => {
                            for point in hist.data_points() {
                                push(&mut out, name, point.attributes(), point.count() as f64);
                            }
                        }
                        MetricData::Sum(sum) => {
                            for point in sum.data_points() {
                                push(&mut out, name, point.attributes(), point.value());
                            }
                        }
                        MetricData::Gauge(gauge) => {
                            for point in gauge.data_points() {
                                push(&mut out, name, point.attributes(), point.value());
                            }
                        }
                        MetricData::ExponentialHistogram(hist) => {
                            for point in hist.data_points() {
                                push(&mut out, name, point.attributes(), point.count() as f64);
                            }
                        }
                    },
                    AggregatedMetrics::U64(data) => match data {
                        MetricData::Histogram(hist) => {
                            for point in hist.data_points() {
                                push(&mut out, name, point.attributes(), point.count() as f64);
                            }
                        }
                        MetricData::Sum(sum) => {
                            for point in sum.data_points() {
                                push(&mut out, name, point.attributes(), point.value() as f64);
                            }
                        }
                        MetricData::Gauge(gauge) => {
                            for point in gauge.data_points() {
                                push(&mut out, name, point.attributes(), point.value() as f64);
                            }
                        }
                        MetricData::ExponentialHistogram(hist) => {
                            for point in hist.data_points() {
                                push(&mut out, name, point.attributes(), point.count() as f64);
                            }
                        }
                    },
                    AggregatedMetrics::I64(data) => match data {
                        MetricData::Histogram(hist) => {
                            for point in hist.data_points() {
                                push(&mut out, name, point.attributes(), point.count() as f64);
                            }
                        }
                        MetricData::Sum(sum) => {
                            for point in sum.data_points() {
                                push(&mut out, name, point.attributes(), point.value() as f64);
                            }
                        }
                        MetricData::Gauge(gauge) => {
                            for point in gauge.data_points() {
                                push(&mut out, name, point.attributes(), point.value() as f64);
                            }
                        }
                        MetricData::ExponentialHistogram(hist) => {
                            for point in hist.data_points() {
                                push(&mut out, name, point.attributes(), point.count() as f64);
                            }
                        }
                    },
                }
            }
        }
    }
    out
}

/// The metric names the batch carries — the failure message of a missing metric.
fn names(points: &[Point]) -> Vec<&str> {
    let mut names: Vec<&str> = points.iter().map(|point| point.metric.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// The summed value of every data point matching `wanted`.
fn total(points: &[Point], wanted: impl Fn(&Point) -> bool) -> f64 {
    points.iter().filter(|point| wanted(point)).map(|point| point.value).sum()
}

fn harness() -> egui_kittest::Harness<'static, SlideApp> {
    egui_kittest::Harness::builder().with_size(egui::vec2(1200.0, 800.0)).build_ui_state(
        |ui, app: &mut SlideApp| {
            app.logic(ui.ctx());
            app.ui(ui);
        },
        SlideApp::new(Session::with_new(), Services::default()),
    )
}

/// A painted frame must report its wall time, its layout/present phases and the delta to the
/// previous frame. Without this the report cannot answer "is the editor at 60 Hz".
#[test]
fn painted_frames_emit_their_duration_delta_and_phases() {
    let _reading = reader();
    let mut harness = harness();
    harness.run_steps(3);
    let points = points();

    // A histogram data point carries the count of samples behind it: three frames must add up to
    // at least one painted frame (the first pass may be discarded while the fonts install).
    assert!(
        total(&points, |point| point.metric == sc::M_FRAME_DURATION) >= 1.0,
        "no {} data point after three frames: {:?}",
        sc::M_FRAME_DURATION,
        names(&points)
    );
    for phase in [sc::P_BUILD, sc::P_PRESENT] {
        assert!(
            total(&points, |point| point.metric == sc::M_FRAME_PHASE && point.attr(sc::A_PHASE) == Some(phase)) >= 1.0,
            "no {} data point for phase {phase}: {:?}",
            sc::M_FRAME_PHASE,
            names(&points)
        );
    }
    // `frame.delta` needs a previous frame: three frames are two intervals. It is a histogram of
    // one sample per interval, so the data point exists once at least one interval happened.
    assert!(
        total(&points, |point| point.metric == sc::M_FRAME_DELTA) >= 1.0,
        "no {} data point after three frames: {:?}",
        sc::M_FRAME_DELTA,
        names(&points)
    );
}
