//! The deterministic 1,000-frame A/B harness: one mutation per frame, official OTLP output.
//!
//! Run it explicitly — it is `#[ignore]` because 1,000 frames of a real raster at 300% zoom are
//! seconds, not milliseconds:
//!
//! ```text
//! DECKCRAFT_OTEL_AB=1 cargo test -p deckcraft-ui-egui --test otel_ab -- --ignored --nocapture
//! ```
//!
//! `DECKCRAFT_TEST_VARIANT` names the run (`candidate` by default) and `DECKCRAFT_OTEL_DIR` picks
//! the output directory (`target/otel/<variant>` by default). Run it once per variant, then:
//!
//! ```text
//! cargo xtask otel-report target/otel/candidate --baseline target/otel/baseline
//! ```
//!
//! # Why one event per request
//!
//! `ui.pointer` collapses every event in a request into one paint and records the extras as
//! `interaction.coalesced_events`. A whole gesture in one request therefore prices one frame, not
//! the gesture — the collapsed-gesture trap. This harness sends **one event per request**, so
//! `coalesced` must be 0; a non-zero value invalidates the run before its numbers are trusted.
//!
//! # What the mutation is
//!
//! A rectangle is inserted, then a single drag gesture is fed one event per frame: `down` during
//! warmup, 1,000 `drag` events (0.1 pt each, snapping disabled with Cmd), `up` after the measured
//! window. Every measured frame must bump the document revision and leave it dirty; a frame whose
//! drag did not change the document is a false negative, not a fast frame.
//!
//! # What it cannot prove
//!
//! Frame durations are wall time around `SlideApp::ui`. They do not include the compositor's
//! presentation, so tearing and perceived smoothness remain proxies (`otel_pacing`). CPU time per
//! phase is not emitted: the crate has no thread-CPU clock without `unsafe`, so `cpu/wall` is
//! `n/a` in the report.

#![allow(clippy::unwrap_used, clippy::expect_used)] // a test: a failed expectation is the report

use std::path::{Path, PathBuf};

use deckcraft_engine::Session;
use deckcraft_telemetry::sc;
use deckcraft_ui_egui::{Services, SlideApp};
use egui::vec2;
use egui_kittest::Harness;
use opentelemetry::KeyValue;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::transform::common::tonic::ResourceAttributesWithSchema;
use opentelemetry_proto::transform::trace::tonic::group_spans_by_resource_and_scope;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use serde_json::json;

/// Frames painted before the measured window, per §8.3.
const WARMUPS: usize = 10;
/// Frames in the measured window, per §8.3. `DECKCRAFT_OTEL_AB_FRAMES` lowers it for a smoke run;
/// the gate always uses the default.
const FRAMES: usize = 1_000;
/// The fixed gesture: a rectangle dragged 0.1 pt per frame. At 300% zoom that is 0.3 px — enough to
/// replace the slide `Arc` and miss the texture cache, without walking the shape off the slide.
const DRAG_STEP_PT: f64 = 0.1;
/// The drag starts here; the rectangle is inserted around this point.
const DRAG_START: (f64, f64) = (200.0, 150.0);

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).ok().filter(|value| !value.trim().is_empty()).unwrap_or_else(|| default.to_owned())
}

fn frames() -> usize {
    std::env::var("DECKCRAFT_OTEL_AB_FRAMES").ok().and_then(|value| value.trim().parse().ok()).unwrap_or(FRAMES).max(10)
}

/// The workspace root: `CARGO_MANIFEST_DIR` is `<root>/crates/ui-egui`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).expect("workspace root").to_path_buf()
}

fn output_dir(variant: &str) -> PathBuf {
    match std::env::var("DECKCRAFT_OTEL_DIR") {
        Ok(value) if !value.trim().is_empty() => {
            let path = PathBuf::from(value);
            if path.is_absolute() { path } else { workspace_root().join(path) }
        }
        _ => workspace_root().join("target/otel").join(variant),
    }
}

/// The resource every record carries: the identity that makes two runs comparable (or not).
fn resource(variant: &str) -> Resource {
    Resource::builder()
        .with_service_name(sc::SERVICE_NAME_VALUE)
        .with_attributes(vec![
            KeyValue::new(sc::SERVICE_VERSION, env!("CARGO_PKG_VERSION")),
            KeyValue::new(sc::DEPLOYMENT_ENVIRONMENT_NAME, sc::V_ENV_TEST),
            KeyValue::new(sc::A_BUILD_PROFILE, if cfg!(debug_assertions) { "debug" } else { "release" }),
            KeyValue::new(sc::A_TEST_RUN_ID, "otel-ab-1000"),
            KeyValue::new(sc::A_TEST_VARIANT, variant.to_owned()),
            KeyValue::new(sc::A_TEST_ZOOM, 3.0),
            KeyValue::new(sc::A_HW_GPU_ADAPTER, env_or("DECKCRAFT_GPU_ADAPTER", "kittest")),
            KeyValue::new(sc::A_RENDERER_BACKEND, env_or("DECKCRAFT_RENDERER", "kittest-wgpu")),
            KeyValue::new(sc::A_DISPLAY_REFRESH_HZ, 60.0),
            KeyValue::new(sc::A_DEVICE_PIXEL_RATIO, 1.0),
        ])
        .build()
}

/// Send one pointer event as its own request: one event, one paint, `coalesced` stays zero.
///
/// `cmd` disables smart guides on a drag (the drag would otherwise snap back to a neighbour and
/// the 0.1 pt step would be lost); it must be **off** for the press, because Cmd-click toggles the
/// selection and would deselect the rectangle the gesture is about to move.
fn pointer(harness: &mut Harness<'_, SlideApp>, kind: &str, x: f64, y: f64, cmd: bool) {
    let mods = if cmd { json!({"cmd": true}) } else { json!({}) };
    let (request, _reply) = deckcraft_ui_egui::ControlRequest::new("ui.pointer", json!({"events": [{"kind": kind, "x": x, "y": y, "mods": mods}]}));
    let ctx = harness.ctx.clone();
    let outcome = deckcraft_ui_egui::control::handle(harness.state_mut(), &ctx, &request);
    let deckcraft_ui_egui::control::Outcome::Done(reply) = outcome else {
        panic!("ui.pointer must return a value");
    };
    assert_eq!(reply.get("ok"), Some(&json!(true)), "the pointer event was rejected: {reply}");
}

/// A point read from a metric data point, with the attributes flattened to `key=value`.
struct Point {
    name: String,
    attrs: Vec<(String, String)>,
    value: f64,
}

impl Point {
    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str())
    }
}

/// Every counter and histogram count the in-memory exporter holds, flattened.
fn points(metrics: &[opentelemetry_sdk::metrics::data::ResourceMetrics]) -> Vec<Point> {
    let mut out = Vec::new();
    for batch in metrics {
        for scope in batch.scope_metrics() {
            for metric in scope.metrics() {
                let name = metric.name().to_owned();
                match metric.data() {
                    AggregatedMetrics::U64(data) => match data {
                        MetricData::Sum(sum) => {
                            for point in sum.data_points() {
                                out.push(Point {
                                    name: name.clone(),
                                    attrs: point.attributes().map(|kv| (kv.key.to_string(), kv.value.to_string())).collect(),
                                    value: point.value() as f64,
                                });
                            }
                        }
                        MetricData::Histogram(histogram) => {
                            for point in histogram.data_points() {
                                out.push(Point {
                                    name: name.clone(),
                                    attrs: point.attributes().map(|kv| (kv.key.to_string(), kv.value.to_string())).collect(),
                                    value: point.count() as f64,
                                });
                            }
                        }
                        _ => {}
                    },
                    AggregatedMetrics::F64(data) => match data {
                        MetricData::Histogram(histogram) => {
                            for point in histogram.data_points() {
                                out.push(Point {
                                    name: name.clone(),
                                    attrs: point.attributes().map(|kv| (kv.key.to_string(), kv.value.to_string())).collect(),
                                    value: point.count() as f64,
                                });
                            }
                        }
                        MetricData::Sum(sum) => {
                            for point in sum.data_points() {
                                out.push(Point {
                                    name: name.clone(),
                                    attrs: point.attributes().map(|kv| (kv.key.to_string(), kv.value.to_string())).collect(),
                                    value: point.value(),
                                });
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
    }
    out
}

fn total(points: &[Point], wanted: impl Fn(&Point) -> bool) -> f64 {
    points.iter().filter(|point| wanted(point)).map(|point| point.value).sum()
}

/// Write one signal as the OTLP File Exporter format: one top-level object per line.
fn write_json_line(path: &Path, value: &impl serde::Serialize) {
    let line = serde_json::to_string(value).expect("OTLP/JSON");
    std::fs::write(path, format!("{line}\n")).expect("write the OTLP file");
}

#[test]
#[ignore = "1,000 frames: run with DECKCRAFT_OTEL_AB=1 cargo test -p deckcraft-ui-egui --test otel_ab -- --ignored --nocapture"]
fn a_thousand_drag_frames_export_official_otlp() {
    if std::env::var("DECKCRAFT_OTEL_AB").as_deref() != Ok("1") {
        eprintln!("otel_ab: DECKCRAFT_OTEL_AB is not 1; skipping the 1,000-frame run");
        return;
    }
    let variant = env_or("DECKCRAFT_TEST_VARIANT", sc::V_CANDIDATE);
    let frames = frames();
    let dir = output_dir(&variant);
    std::fs::create_dir_all(&dir).expect("create the OTLP output directory");

    // The SDK at the test boundary, exactly as the app wires it: providers first, then the shared
    // instrument handles. A handle built before `set_meter_provider` would stay no-op forever.
    let resource = resource(&variant);
    let span_exporter = InMemorySpanExporter::default();
    let tracer_provider = SdkTracerProvider::builder().with_batch_exporter(span_exporter.clone()).with_resource(resource.clone()).build();
    opentelemetry::global::set_tracer_provider(tracer_provider.clone());
    let metric_exporter = InMemoryMetricExporter::default();
    let meter_provider = SdkMeterProvider::builder().with_periodic_exporter(metric_exporter.clone()).with_resource(resource.clone()).build();
    opentelemetry::global::set_meter_provider(meter_provider.clone());
    deckcraft_telemetry::init_instruments();

    let mut session = Session::new();
    deckcraft_engine::sample::open_sample(&mut session).expect("the sample deck");
    let mut app = SlideApp::new(session, Services::default());
    // The resource says 3.0; the app must actually render at 3.0 or the report's ms/Mpx is a lie.
    app.ui.zoom = Some(3.0);
    let mut harness = Harness::builder().with_size(vec2(1600.0, 1000.0)).with_step_dt(1.0 / 60.0).build_ui_state(
        |ui, app: &mut SlideApp| {
            app.logic(ui.ctx());
            app.ui(ui);
        },
        app,
    );
    harness.run_steps(WARMUPS);

    // A rectangle around the drag origin, so the gesture has something to move.
    harness.state_mut().run("shape.insert", json!({"preset": "rect", "rect": [100.0, 100.0, 300.0, 200.0]})).expect("insert the dragged rectangle");
    harness.run_steps(2);
    assert!(harness.state().session.doc().expect("a document").is_dirty(), "the insert must dirty the document");

    // Start the gesture in the warmup: `down` selects, the first `drag` clears the drag threshold.
    pointer(&mut harness, "down", DRAG_START.0, DRAG_START.1, false);
    harness.run_steps(1);
    pointer(&mut harness, "drag", DRAG_START.0 + 2.0, DRAG_START.1, true);
    harness.run_steps(1);

    // The measured window: one drag event per frame, each one a real document mutation.
    let mut last_x = DRAG_START.0 + 2.0;
    for frame in 0..frames {
        let before = harness.state().session.doc().expect("a document").revision;
        last_x = DRAG_START.0 + 2.0 + (frame as f64 + 1.0) * DRAG_STEP_PT;
        pointer(&mut harness, "drag", last_x, DRAG_START.1, true);
        harness.run_steps(1);
        let doc = harness.state().session.doc().expect("a document");
        assert!(doc.revision > before, "frame {frame}: the drag did not mutate the document (revision {} -> {})", before, doc.revision);
        assert!(doc.is_dirty(), "frame {frame}: the document is not dirty after a mutation");
    }
    pointer(&mut harness, "up", last_x, DRAG_START.1, false);
    harness.run_steps(1);

    tracer_provider.force_flush().expect("flush the span batch processor");
    meter_provider.force_flush().expect("flush the metric reader");
    let spans = span_exporter.get_finished_spans().expect("read the exported spans");
    let metrics = metric_exporter.get_finished_metrics().expect("read the exported metrics");

    let frame_spans = spans.iter().filter(|span| span.name.as_ref() == sc::S_FRAME).count();
    let interactions = spans.iter().filter(|span| span.name.as_ref() == sc::S_INTERACTION).count();
    assert!(frame_spans >= frames, "only {frame_spans} `{}` spans for {frames} measured frames", sc::S_FRAME);
    assert!(interactions >= frames, "only {interactions} `{}` spans for {frames} measured interactions", sc::S_INTERACTION);

    let points = points(&metrics);
    let coalesced = total(&points, |point| point.name == sc::M_INTERACTION_COALESCED);
    assert_eq!(coalesced, 0.0, "the harness sends one event per request; a non-zero coalesced count invalidates the run");
    let hits = total(&points, |point| point.name == sc::M_CACHE && point.attr(sc::A_CACHE_RESULT) == Some(sc::V_HIT));
    let misses = total(&points, |point| point.name == sc::M_CACHE && point.attr(sc::A_CACHE_RESULT) == Some(sc::V_MISS));
    assert!(hits > 0.0, "no cache hit was recorded; the warmup frames should have reused the texture");
    assert!(misses >= frames as f64, "only {misses} cache misses for {frames} mutating frames");
    let painted = total(&points, |point| point.name == sc::M_FRAME_DURATION);
    assert!(painted >= frames as f64, "the frame histogram saw {painted} samples for {frames} frames");

    // The official OTLP File Exporter shape: one object per line, one signal per file. The same
    // proto transforms the app's exporters use, so the report and a collector read identical bytes.
    let resource_attributes = ResourceAttributesWithSchema::from(&resource);
    let traces = ExportTraceServiceRequest { resource_spans: group_spans_by_resource_and_scope(spans, &resource_attributes) };
    write_json_line(&dir.join("traces.jsonl"), &traces);
    let mut metric_lines = String::new();
    for batch in &metrics {
        let request = ExportMetricsServiceRequest::from(batch);
        metric_lines.push_str(&serde_json::to_string(&request).expect("OTLP/JSON metrics"));
        metric_lines.push('\n');
    }
    std::fs::write(dir.join("metrics.jsonl"), metric_lines).expect("write metrics.jsonl");

    tracer_provider.shutdown().expect("shut the tracer provider down");
    meter_provider.shutdown().expect("shut the meter provider down");

    println!(
        "otel_ab: variant={variant} frames={frames} frame_spans={frame_spans} interactions={interactions} coalesced={coalesced} cache_hit={hits} cache_miss={misses} painted={painted} dir={}",
        dir.display()
    );
}
