//! Guards the typed instrument handles.
//!
//! The crate deliberately has no SDK (it is L0 and wasm-safe), so these tests build the handles
//! from a **no-op** meter: they prove that every metric in the P0 table has a handle, that the
//! handle was built with the semconv name/unit/bounds (a wrong name or a missing bound would not
//! compile differently, but a missing `M_*` constant would), and that each handle accepts a
//! record at the unit the table documents. Observing exported values is the SDK's job and belongs
//! to the app-level tests.

use deckcraft_telemetry::instruments::Instruments;
use deckcraft_telemetry::sc;
use opentelemetry::InstrumentationScope;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Meter, MeterProvider, NoopMeterProvider};

/// A meter exactly as the app sees it before the SDK is configured: backed by the no-op provider.
fn noop_meter() -> Meter {
    let scope = InstrumentationScope::builder(sc::SCOPE_NAME).with_version(sc::SCOPE_VERSION).build();
    NoopMeterProvider::new().meter_with_scope(scope)
}

#[test]
fn every_histogram_counter_and_gauge_accepts_a_record() {
    let meter = noop_meter();
    let i = Instruments::new(&meter);

    // Frame family — the four statistics the report derives (p50/p90/p99/worst) all come from
    // these samples, so each one must be recorded with the unit the table declares.
    i.frame_duration.record(sc::FRAME_BUDGET_MS, &[KeyValue::new(sc::A_FRAME_KIND, "interactive")]);
    i.frame_phase.record(4.0, &[KeyValue::new(sc::A_PHASE, sc::P_RASTER)]);
    i.frame_phase_cpu.record(12.0, &[KeyValue::new(sc::A_PHASE, sc::P_RASTER)]);
    i.frame_delta.record(16.7, &[KeyValue::new(sc::A_FRAME_KIND, "interactive")]);
    i.frame_janky.add(1, &[KeyValue::new(sc::A_JANK_CLASS, sc::V_JANK_MISSED_VSYNC)]);
    i.pacing_error.record(-0.1, &[KeyValue::new(sc::A_DISPLAY_PRESENT_MODE, "fifo")]);
    i.frame_late.add(1, &[]);
    i.frame_duplicate.add(1, &[]);
    i.frame_split.add(1, &[]);

    // Interaction family (INP). The three phases are one histogram with an `inp.phase` attribute,
    // never three instruments.
    i.interaction_duration.record(25.0, &[KeyValue::new(sc::A_INTERACTION_TYPE, "drag")]);
    i.interaction_phase.record(0.8, &[KeyValue::new(sc::A_INP_PHASE, sc::P_INPUT_DELAY)]);
    i.interaction_count.add(1, &[KeyValue::new(sc::A_INTERACTION_TYPE, "drag")]);
    i.interaction_coalesced.add(0, &[KeyValue::new(sc::A_INTERACTION_TYPE, "drag")]);

    // Raster and task attribution.
    i.raster_duration.record(18.4, &[KeyValue::new(sc::A_RENDER_SCALE, 3.0)]);
    i.raster_pixels.record(2.94, &[KeyValue::new(sc::A_RENDER_SCALE, 3.0)]);
    i.task_duration.record(294.0, &[KeyValue::new(sc::A_TASK_NAME, sc::V_TASK_EXPORT_PDF)]);

    // Texture cache and payload.
    i.texture_cache.add(1, &[KeyValue::new(sc::A_CACHE_RESULT, sc::V_HIT)]);
    i.texture_thrash.add(1, &[KeyValue::new(sc::A_SLIDE_INDEX, 3_i64)]);
    i.texture_bytes.record(5_242_880.0, &[KeyValue::new(sc::A_CACHE_KIND, sc::V_CANVAS)]);

    // GPU attribution and repaint sources.
    i.gpu_frame_time.record(0.7, &[]);
    i.gpu_utilization.record(0.02, &[]);
    i.gpu_memory_used.record(569_376_768.0, &[]);
    i.repaint_requested.add(1, &[KeyValue::new(sc::A_REPAINT_SOURCE, sc::V_REPAINT_CONTROL)]);

    // Self-health: a non-zero `dropped_records` invalidates the whole measurement.
    i.dropped_records.add(0, &[]);
    i.export_duration.record(1.2, &[]);

    // Startup, load and footprint.
    i.time_to_first_frame.record(120.0, &[]);
    i.largest_paint.record(140.0, &[KeyValue::new(sc::A_CONTENT_TYPE, sc::V_CONTENT_SLIDE_CANVAS)]);
    i.memory_rss.record(268_435_456.0, &[]);
    i.binary_size.record(18_000_000.0, &[KeyValue::new(sc::A_ARTIFACT, sc::V_ARTIFACT_NATIVE)]);
}

#[test]
fn the_global_handle_set_is_created_once_and_init_is_idempotent() {
    // `init_instruments()` is the app's early-binding hook: meters grabbed before the SDK provider
    // is installed stay no-op forever, so the app calls it once, right after `set_meter_provider`.
    // Calling it twice (or calling the accessor first) must not swap the handles underneath the
    // call sites that already hold them.
    let first = deckcraft_telemetry::instruments();
    assert!(std::ptr::eq(first, deckcraft_telemetry::instruments()), "accessor must be a OnceLock");

    deckcraft_telemetry::init_instruments();
    assert!(std::ptr::eq(first, deckcraft_telemetry::instruments()), "init_instruments must be idempotent");
}
