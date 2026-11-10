//! The overhead guard: instrumentation off by default must stay invisible, and the SDK path must
//! not move a frame's median by more than 5%.
//!
//! Two independent checks, because they fail differently:
//!
//! 1. With no provider installed (`DECKCRAFT_OTEL` unset in the app), 10,000 records must finish in
//!    under 100 ms. The global provider is the API's no-op one; a regression here means a call site
//!    started formatting strings or building a `Vec` before the no-op check.
//! 2. A paired on/off run of the same synthetic workload, using `Instruments::new` against a no-op
//!    meter and against a real `SdkMeterProvider`. The median of 300 iterations must stay within
//!    ±5%. This is the instrumentation's cost, not the app's frame time: the workload is fixed and
//!    the only difference is whether the records land in an SDK pipeline.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use deckcraft_telemetry::instruments::Instruments;
use deckcraft_telemetry::sc;
use opentelemetry::KeyValue;
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};

/// The record budget for the no-op path: 10,000 records in 100 ms is 10 µs each, two orders of
/// magnitude above what a no-op instrument costs. A failure means real work was added, not noise.
const NOOP_BUDGET: Duration = Duration::from_millis(100);
const NOOP_RECORDS: usize = 10_000;

/// Paired A/B: 300 iterations of ~1 ms of work, each recording ten instruments. One millisecond is
/// far below a real 12 ms frame, so the ±5% gate is conservative: if instrumentation costs 5% of a
/// 1 ms slice, it costs 0.4% of a real frame.
const ITERATIONS: usize = 300;
const BUSY_ROUNDS: u64 = 5_000_000;

/// Work the optimizer cannot delete, standing in for one frame of real computation.
fn busy(seed: u64) -> u64 {
    let mut accumulator = seed;
    for round in 0..BUSY_ROUNDS {
        accumulator = accumulator.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(round);
    }
    std::hint::black_box(accumulator)
}

/// One iteration: the same ten records a painted frame emits, with their attributes.
fn record_frame(instruments: &Instruments, iteration: usize) {
    instruments.frame_duration.record(16.0, &[]);
    instruments.frame_phase.record(3.0, &[KeyValue::new(sc::A_PHASE, sc::P_BUILD)]);
    instruments.frame_delta.record(16.7, &[]);
    instruments.frame_janky.add(0, &[KeyValue::new(sc::A_JANK_CLASS, sc::V_JANK_MISSED_VSYNC)]);
    instruments.texture_cache.add(1, &[KeyValue::new(sc::A_CACHE_KIND, sc::V_CANVAS), KeyValue::new(sc::A_CACHE_RESULT, sc::V_HIT)]);
    instruments.raster_duration.record(3.0, &[KeyValue::new(sc::A_SLIDE_INDEX, iteration as i64)]);
    instruments.raster_pixels.record(3.3, &[KeyValue::new(sc::A_RENDER_SCALE, 3.0)]);
    instruments.texture_bytes.record(13_300_000.0, &[KeyValue::new(sc::A_CACHE_KIND, sc::V_CANVAS)]);
    instruments.interaction_count.add(1, &[KeyValue::new(sc::A_INTERACTION_TYPE, sc::V_INTERACTION_DRAG)]);
    instruments.repaint_requested.add(1, &[KeyValue::new(sc::A_REPAINT_SOURCE, sc::V_REPAINT_CONTROL)]);
}

fn run(instruments: &Instruments) -> Vec<Duration> {
    let mut samples = Vec::with_capacity(ITERATIONS);
    for iteration in 0..ITERATIONS {
        let start = Instant::now();
        std::hint::black_box(busy(iteration as u64));
        record_frame(instruments, iteration);
        samples.push(start.elapsed());
    }
    samples
}

fn p50(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

/// The app's default: no provider installed. Every handle resolves to the API's no-op provider,
/// so 10,000 records must not allocate, format or lock anything meaningful.
#[test]
fn ten_thousand_records_without_a_provider_take_under_100ms() {
    let instruments = deckcraft_telemetry::instruments();
    let start = Instant::now();
    for iteration in 0..NOOP_RECORDS {
        record_frame(instruments, iteration);
    }
    let elapsed = start.elapsed();
    assert!(elapsed < NOOP_BUDGET, "{NOOP_RECORDS} no-op records took {elapsed:?}, budget {NOOP_BUDGET:?}");
}

/// Paired on/off: the same workload with a no-op meter and with a real SDK meter. The median must
/// stay within ±5%; the SDK path is allowed to cost something, not to move the frame.
#[test]
fn a_paired_on_off_run_keeps_the_median_within_five_percent() {
    // The global meter is the no-op one: no test in this binary installs a global provider.
    let off_meter = opentelemetry::global::meter("deckcraft-overhead-off");
    let off = Instruments::new(&off_meter);

    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder().with_periodic_exporter(exporter.clone()).build();
    let on_meter = provider.meter("deckcraft-overhead-on");
    let on = Instruments::new(&on_meter);

    // Warm up both paths so the first SDK aggregation does not land in the measured p50.
    std::hint::black_box(run(&off));
    std::hint::black_box(run(&on));

    let off_median = p50(run(&off));
    let on_median = p50(run(&on));
    provider.force_flush().expect("flush the in-memory reader");

    let ratio = on_median.as_secs_f64() / off_median.as_secs_f64();
    assert!(
        (0.95..=1.05).contains(&ratio),
        "instrumentation moved the paired median: off={off_median:?} on={on_median:?} ratio={ratio:.3} (gate 0.95..=1.05)"
    );
}
