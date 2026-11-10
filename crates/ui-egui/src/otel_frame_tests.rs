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
use serde_json::json;

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
    /// The histogram sample count (the accumulation of a sum or gauge, for those kinds).
    value: f64,
    /// The histogram's recorded values summed. Zero for counters, where `value` is the sum.
    sum: f64,
}

impl Point {
    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str())
    }
}

fn push<'a>(out: &mut Vec<Point>, metric: &str, attrs: impl Iterator<Item = &'a KeyValue>, value: f64) {
    out.push(Point { metric: metric.to_owned(), attrs: attrs.map(|kv| (kv.key.to_string(), kv.value.to_string())).collect(), value, sum: 0.0 });
}

/// A histogram data point: its sample count and the sum of the samples, so a test can tell "it
/// recorded" from "it recorded a real number".
fn push_hist<'a>(out: &mut Vec<Point>, metric: &str, attrs: impl Iterator<Item = &'a KeyValue>, count: f64, sum: f64) {
    out.push(Point { metric: metric.to_owned(), attrs: attrs.map(|kv| (kv.key.to_string(), kv.value.to_string())).collect(), value: count, sum });
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
                                push_hist(&mut out, name, point.attributes(), point.count() as f64, point.sum());
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
                                push_hist(&mut out, name, point.attributes(), point.count() as f64, point.sum());
                            }
                        }
                    },
                    AggregatedMetrics::U64(data) => match data {
                        MetricData::Histogram(hist) => {
                            for point in hist.data_points() {
                                push_hist(&mut out, name, point.attributes(), point.count() as f64, point.sum() as f64);
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
                                push_hist(&mut out, name, point.attributes(), point.count() as f64, point.sum() as f64);
                            }
                        }
                    },
                    AggregatedMetrics::I64(data) => match data {
                        MetricData::Histogram(hist) => {
                            for point in hist.data_points() {
                                push_hist(&mut out, name, point.attributes(), point.count() as f64, point.sum() as f64);
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
                                push_hist(&mut out, name, point.attributes(), point.count() as f64, point.sum() as f64);
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

/// The sum of the recorded samples of every histogram data point matching `wanted` — the actual
/// milliseconds, not the number of samples.
fn total_sum(points: &[Point], wanted: impl Fn(&Point) -> bool) -> f64 {
    points.iter().filter(|point| wanted(point)).map(|point| point.sum).sum()
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

/// A pointer gesture driven through the control channel must produce one INP sample with all three
/// phases. This is the route where the collapsed-gesture trap lives: `ui.pointer` hands the engine
/// three events in one request, the app handles them in one `logic` pass and one frame paints the
/// result — so the sample must be closed by the frame that painted it, not by a later frame.
#[test]
fn a_pointer_gesture_through_the_control_channel_reports_inp_and_its_three_phases() {
    let _reading = reader();
    let mut harness = harness();
    harness.run_steps(3);

    let before = points();
    let duration_before = total(&before, |point| point.metric == sc::M_INTERACTION_DURATION);
    let phase_before = total(&before, |point| point.metric == sc::M_INTERACTION_PHASE);
    let coalesced_before = total(&before, |point| point.metric == sc::M_INTERACTION_COALESCED);

    let (request, _reply) = crate::control::ControlRequest::new(
        "ui.pointer",
        json!({"events": [
            {"kind": "down", "x": 480.0, "y": 200.0},
            {"kind": "drag", "x": 520.0, "y": 220.0},
            {"kind": "up", "x": 520.0, "y": 220.0},
        ]}),
    );
    let ctx = harness.ctx.clone();
    let outcome = crate::control::handle(harness.state_mut(), &ctx, &request);
    assert!(matches!(outcome, crate::control::Outcome::Done(_)), "the control request must be accepted");
    harness.run_steps(2);

    let after = points();
    assert!(
        total(&after, |point| point.metric == sc::M_INTERACTION_DURATION && point.attr(sc::A_INTERACTION_TYPE) == Some("drag")) > duration_before,
        "no {} sample for the gesture: {:?}",
        sc::M_INTERACTION_DURATION,
        names(&after)
    );
    for phase in [sc::P_INPUT_DELAY, sc::P_PROCESSING, sc::P_PRESENTATION] {
        assert!(
            total(&after, |point| point.metric == sc::M_INTERACTION_PHASE && point.attr(sc::A_INP_PHASE) == Some(phase)) > phase_before,
            "no {} sample for inp.phase={phase}: {:?}",
            sc::M_INTERACTION_PHASE,
            names(&after)
        );
    }
    // The gate: three engine events became one paint, so two events were coalesced. A run whose
    // coalesced count is above zero cannot be used to price per-frame pointer cost.
    assert!(
        total(&after, |point| point.metric == sc::M_INTERACTION_COALESCED) - coalesced_before >= 2.0,
        "three events painted by one frame must report two coalesced events: {:?}",
        names(&after)
    );
    // A sample of 0 ms is not a fast interaction, it is a broken clock: two different time bases
    // (egui's input time and the process clock) subtract to a negative number that the phase
    // clamp turns into zero. The sample must carry real milliseconds.
    let drag_ms = total_sum(&after, |point| point.metric == sc::M_INTERACTION_DURATION && point.attr(sc::A_INTERACTION_TYPE) == Some("drag"))
        - total_sum(&before, |point| point.metric == sc::M_INTERACTION_DURATION && point.attr(sc::A_INTERACTION_TYPE) == Some("drag"));
    assert!(drag_ms > 0.0, "the INP sample is {drag_ms} ms — the phases were clamped from a clock mismatch: {:?}", names(&after));
}

/// A frame that misses its vsync must emit a pacing error and the `late` tearing proxy. The
/// display interval is forced absurdly small so every harness frame is late: the verdict is
/// arithmetic on real timestamps, so this exercises the emission, not a stub.
#[test]
fn a_late_frame_emits_a_pacing_error_and_the_tearing_proxy() {
    let _reading = reader();
    let mut harness = harness();
    harness.run_steps(2);
    // 0.01 ms per display interval: any real frame exceeds one and a half intervals.
    harness.state_mut().display_interval_ms = 0.01;

    let before = points();
    let error_before = total(&before, |point| point.metric == sc::M_PACING_ERROR);
    let late_before = total(&before, |point| point.metric == sc::M_FRAME_LATE);
    harness.run_steps(3);
    let after = points();

    assert!(
        total(&after, |point| point.metric == sc::M_PACING_ERROR) - error_before >= 3.0,
        "three late frames must each record a pacing error: {:?}",
        names(&after)
    );
    assert!(
        total(&after, |point| point.metric == sc::M_FRAME_LATE) - late_before >= 3.0,
        "three late frames must each count as late: {:?}",
        names(&after)
    );
}

/// Startup is two different promises: the first frame is painted, and the app accepts its first
/// input. Both are once per process and both are measured from the instant `main()` took, not from
/// the app's construction — the wait before it is part of what the user felt.
#[test]
fn startup_reports_time_to_first_frame_and_to_first_accepted_input() {
    let _reading = reader();
    let mut harness = harness();
    // The process "started" a second ago: a sample must exist and be at least that large.
    let started = web_time::Instant::now() - std::time::Duration::from_secs(1);
    harness.state_mut().mark_process_start(started);

    let before = points();
    let frame_before = total(&before, |point| point.metric == sc::M_TTF_FIRST_FRAME);
    let input_before = total(&before, |point| point.metric == sc::M_TTI_INTERACTIVE);
    harness.run_steps(2);

    let (request, _reply) = crate::control::ControlRequest::new("ui.key", json!({"key": "right"}));
    let ctx = harness.ctx.clone();
    let outcome = crate::control::handle(harness.state_mut(), &ctx, &request);
    assert!(matches!(outcome, crate::control::Outcome::Done(_)), "the control request must be accepted");
    harness.run_steps(2);

    let after = points();
    assert!(
        total(&after, |point| point.metric == sc::M_TTF_FIRST_FRAME) - frame_before >= 1.0,
        "no {} sample: {:?}",
        sc::M_TTF_FIRST_FRAME,
        names(&after)
    );
    assert!(
        total(&after, |point| point.metric == sc::M_TTI_INTERACTIVE) - input_before >= 1.0,
        "no {} sample: {:?}",
        sc::M_TTI_INTERACTIVE,
        names(&after)
    );
}

/// Exporting a PDF is the 294 ms main-thread stall `cargo xtask bench` measured and nobody could
/// name. It must land in `task.duration` under `task.name=export.pdf`, so a slow frame can be
/// attributed to the export that blocked it instead of to "the UI".
#[test]
fn a_pdf_export_is_timed_as_a_named_task() {
    let _reading = reader();
    let path = std::env::temp_dir().join(format!("deckcraft-otel-task-{}.pdf", std::process::id()));
    let target = path.to_string_lossy().to_string();
    let mut app = SlideApp::new(Session::with_new(), Services::default());
    app.services.pick_save = Some(Box::new(move |_suggested: &str| Some(target.clone())));
    let mut harness = egui_kittest::Harness::builder().with_size(egui::vec2(1200.0, 800.0)).build_ui_state(
        |ui, app: &mut SlideApp| {
            app.logic(ui.ctx());
            app.ui(ui);
        },
        app,
    );
    harness.run_steps(2);

    let before = points();
    let export_before = total(&before, |point| point.metric == sc::M_TASK_DURATION && point.attr(sc::A_TASK_NAME) == Some("export.pdf"));
    assert!(harness.state_mut().export_as("pdf", json!({})), "the export must be written to the picked path");
    let after = points();

    assert!(
        total(&after, |point| point.metric == sc::M_TASK_DURATION && point.attr(sc::A_TASK_NAME) == Some("export.pdf")) > export_before,
        "the export was not timed as a named task: {:?}",
        names(&after)
    );
    let _ = std::fs::remove_file(&path);
}

/// A rejected request must not price an interaction. Opening the INP sample before the events are
/// known to be valid leaves a pending interaction that the next frame happily "paints", recording a
/// latency for a gesture that never happened — a fake sample in the exact tail the report reads.
#[test]
fn a_rejected_pointer_request_records_no_interaction() {
    let _reading = reader();
    let mut harness = harness();
    harness.run_steps(2);

    let (request, _reply) = crate::control::ControlRequest::new("ui.pointer", json!({"events": [{"kind": "wiggle", "x": 1.0, "y": 2.0}]}));
    let ctx = harness.ctx.clone();
    let outcome = crate::control::handle(harness.state_mut(), &ctx, &request);
    let crate::control::Outcome::Done(reply) = outcome else { panic!("a dispatch result is a value") };
    assert_eq!(reply.get("ok"), Some(&json!(false)), "an unknown pointer kind must be rejected: {reply}");
    // The invariant is per app, not per histogram: shared instrumentation cannot attribute a
    // sample to one test while the soak test is recording its own input in another thread.
    assert!(harness.state_mut().interactions.is_empty(), "a rejected request must not leave a pending interaction for the next frame to paint");
    harness.run_steps(2);
}

/// An unchanged slide must hit the texture cache, an edited one must miss *with a reason*, and
/// every phase of the miss (raster, convert, upload) must be timed. A 24 ms miss that reports
/// only its raster is the blind instrument this instrumentation exists to replace.
#[test]
fn an_edited_slide_misses_the_texture_cache_with_a_reason_and_times_every_phase() {
    let _reading = reader();
    let mut harness = harness();
    harness.run_steps(3);

    let before = points();
    let reasoned_before = total(&before, |point| point.metric == sc::M_CACHE && point.attr(sc::A_CACHE_REASON) == Some(sc::V_REASON_SLIDE));

    harness.state_mut().run("shape.insert", json!({"preset": "rect", "rect": [100.0, 100.0, 300.0, 200.0]})).unwrap();
    harness.run_steps(3);

    let after = points();
    let reasoned_after = total(&after, |point| point.metric == sc::M_CACHE && point.attr(sc::A_CACHE_REASON) == Some(sc::V_REASON_SLIDE));
    assert!(
        reasoned_after > reasoned_before,
        "the edit must invalidate the canvas texture with cache.reason={}: {reasoned_before} -> {reasoned_after}",
        sc::V_REASON_SLIDE
    );
    assert!(
        after.iter().any(|point| point.metric == sc::M_CACHE && point.attr(sc::A_CACHE_RESULT) == Some(sc::V_HIT)),
        "no cache hit for the three unchanged frames before the edit: {:?}",
        names(&after)
    );
    for phase in [sc::P_RASTER, sc::P_CONVERT, sc::P_UPLOAD] {
        assert!(
            after.iter().any(|point| point.metric == sc::M_FRAME_PHASE && point.attr(sc::A_PHASE) == Some(phase)),
            "the miss must time its {phase} phase: {:?}",
            names(&after)
        );
    }
}
