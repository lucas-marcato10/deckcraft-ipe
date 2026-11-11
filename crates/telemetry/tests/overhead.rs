//! The overhead guard: instrumentation off by default must stay invisible, and the SDK path must
//! not move a frame's median by more than 5%.
//!
//! Three independent checks, because they fail differently:
//!
//! 1. With no provider installed (`DECKCRAFT_OTEL` unset in the app), 10,000 records must finish in
//!    under 100 ms. The global provider is the API's no-op one; a regression here means a call site
//!    started formatting strings or building a `Vec` before the no-op check.
//! 2. A paired on/off run of the same synthetic workload, using `Instruments::new` against a no-op
//!    meter and against a real `SdkMeterProvider`. The median of 300 interleaved pairs must stay
//!    within ±5%. This is the instrumentation's cost, not the app's frame time: the workload is
//!    fixed and the only difference is whether the records land in an SDK pipeline.
//! 3. With a real SDK meter whose exporter is being flushed concurrently by a collector thread,
//!    10,000 records must still finish under a generous ceiling. The first two checks say what the
//!    instrumentation costs; this one says the SDK's own aggregation lock cannot serialise the UI
//!    thread behind an export. It is a sanity ceiling, not a benchmark.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use deckcraft_telemetry::instruments::Instruments;
use deckcraft_telemetry::sc;
use opentelemetry::KeyValue;
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};

/// The record budget for the no-op path: 10,000 records in 100 ms is 10 µs each, two orders of
/// magnitude above what a no-op instrument costs. A failure means real work was added, not noise.
const NOOP_BUDGET: Duration = Duration::from_millis(100);
const NOOP_RECORDS: usize = 10_000;

/// The contention ceiling: 10,000 records while a collector flushes the same aggregation state in a
/// tight loop must finish in 500 ms. Deliberately loose — it fires if someone introduces a global
/// lock per sample, and it is a sanity ceiling, not a benchmark: never compare machines with it and
/// never use it as an A/B gate.
const CONTENTION_BUDGET: Duration = Duration::from_millis(500);
const CONTENTION_RECORDS: usize = 10_000;

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

/// One iteration of the same synthetic frame, timed on its own instrument set.
fn timed_iteration(instruments: &Instruments, iteration: usize) -> Duration {
    let start = Instant::now();
    std::hint::black_box(busy(iteration as u64));
    record_frame(instruments, iteration);
    start.elapsed()
}

/// The same frame recorded through the no-op meter and then through the SDK meter, back to back, so
/// both halves of the comparison see the same machine state. This is what makes the run *paired*:
/// with the two paths in consecutive blocks, the ratio of medians swings by ±10% on a loaded host
/// (measured: 1.070, 1.098 and 0.916 in eight runs, three outside the ±5% gate) while the interleaved
/// per-pair ratio stayed within ±0.2% in the same eight runs. The block design was measuring drift,
/// not instrumentation cost.
fn timed_pairs(off: &Instruments, on: &Instruments) -> (Vec<Duration>, Vec<Duration>) {
    let mut off_samples = Vec::with_capacity(ITERATIONS);
    let mut on_samples = Vec::with_capacity(ITERATIONS);
    for iteration in 0..ITERATIONS {
        off_samples.push(timed_iteration(off, iteration));
        on_samples.push(timed_iteration(on, iteration));
    }
    (off_samples, on_samples)
}

fn p50(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

/// Median of the per-pair on/off ratios: the robust centre of paired samples, immune to the drift
/// that moves a whole block.
fn median_ratio(off: &[Duration], on: &[Duration]) -> f64 {
    let mut ratios: Vec<f64> = off.iter().zip(on).map(|(off_sample, on_sample)| on_sample.as_secs_f64() / off_sample.as_secs_f64()).collect();
    ratios.sort_by(|a, b| a.partial_cmp(b).expect("no NaN ratios"));
    ratios[ratios.len() / 2]
}

/// The largest `count()` the in-memory exporter reported for `name`. The exporter's default
/// temporality is cumulative, so each export carries the running total and the maximum across
/// batches is the number of samples the SDK actually aggregated.
fn exported_histogram_count(exporter: &InMemoryMetricExporter, name: &str) -> u64 {
    let batches = exporter.get_finished_metrics().expect("read the in-memory exporter");
    let mut total = 0;
    for batch in &batches {
        for scope in batch.scope_metrics() {
            for metric in scope.metrics() {
                if metric.name() != name {
                    continue;
                }
                if let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = metric.data() {
                    for point in histogram.data_points() {
                        total = total.max(point.count());
                    }
                }
            }
        }
    }
    total
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

/// Paired on/off: the same workload with a no-op meter and with a real SDK meter, interleaved pair by
/// pair. The median ratio must stay within ±5%; the SDK path is allowed to cost something, not to
/// move the frame.
#[test]
fn a_paired_on_off_run_keeps_the_median_within_five_percent() {
    // The global meter is the no-op one: no test in this binary installs a global provider.
    let off_meter = opentelemetry::global::meter("deckcraft-overhead-off");
    let off = Instruments::new(&off_meter);

    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder().with_periodic_exporter(exporter.clone()).build();
    let on_meter = provider.meter("deckcraft-overhead-on");
    let on = Instruments::new(&on_meter);

    // Warm up both paths so the first SDK aggregation does not land in the measured median.
    std::hint::black_box(timed_pairs(&off, &on));

    let (off_samples, on_samples) = timed_pairs(&off, &on);
    let ratio = median_ratio(&off_samples, &on_samples);
    let off_median = p50(off_samples);
    let on_median = p50(on_samples);
    provider.force_flush().expect("flush the in-memory reader");

    assert!(
        (0.95..=1.05).contains(&ratio),
        "instrumentation moved the paired median: off={off_median:?} on={on_median:?} ratio={ratio:.3} (gate 0.95..=1.05)"
    );
}

/// The contention ceiling: a real SDK meter, with a second thread forcing the collector to flush the
/// same aggregation state, while 10,000 records land from this thread.
///
/// Deliberately loose — it fires if someone introduces a global lock per sample, not to rank
/// machines. `provider.meter(..)` is a fresh meter on purpose: the crate's `instruments()` handles
/// live behind a process-wide `OnceLock` that other tests may already have resolved.
///
/// The collector does not sleep between flushes. With a 1 ms pause, none of its flushes lands inside
/// a ~200 µs recording window (measured: zero in four consecutive runs), so the timing would be taken
/// with no contention in it at all — a test that cannot go red on the symptom it exists for. Flushing
/// in a tight loop puts 30-39 exports inside the window on this host; the two assertions before the
/// ceiling prove that overlap and that the SDK really aggregated the samples, so the elapsed time is
/// the cost of contended recording and not of a stray sleep.
#[test]
fn recording_while_a_collector_flushes_stays_bounded() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;

    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder().with_periodic_exporter(exporter.clone()).build();
    let meter = provider.meter("contention-test");
    let histogram = meter.f64_histogram("deckcraft.test.frame.duration").build();

    let done = Arc::new(AtomicBool::new(false));
    let flushes = Arc::new(AtomicUsize::new(0));
    let flag = Arc::clone(&done);
    let flush_counter = Arc::clone(&flushes);
    let flushing = provider.clone();
    // Stands in for the SDK thread collecting/exporting while the UI thread records.
    let collector = thread::spawn(move || {
        while !flag.load(Ordering::Relaxed) {
            let _ = flushing.force_flush();
            flush_counter.fetch_add(1, Ordering::Relaxed);
        }
    });

    // Let the collector prove it is scheduled and has exported once before the clock starts.
    while flushes.load(Ordering::Relaxed) == 0 {
        thread::yield_now();
    }
    let flushes_before = flushes.load(Ordering::Relaxed);

    let t0 = Instant::now();
    for i in 0..CONTENTION_RECORDS {
        histogram.record((i % 30) as f64, &[]);
    }
    let elapsed = t0.elapsed();
    let flushes_during = flushes.load(Ordering::Relaxed) - flushes_before;

    done.store(true, Ordering::Relaxed);
    collector.join().expect("collector panicou");
    let _ = provider.shutdown();

    // Validity first: the collector exported while we recorded, and the SDK aggregated every sample.
    // Without both, the elapsed time below would measure nothing and pass for the wrong reason.
    assert!(flushes_during > 0, "no flush landed inside the recording window: no contention was measured");
    let exported = exported_histogram_count(&exporter, "deckcraft.test.frame.duration");
    assert_eq!(
        exported, CONTENTION_RECORDS as u64,
        "the SDK exported {exported} samples, not {CONTENTION_RECORDS}: the timed work never reached the pipeline"
    );

    assert!(elapsed < CONTENTION_BUDGET, "{CONTENTION_RECORDS} records com coletor vivo: {elapsed:?}, budget {CONTENTION_BUDGET:?}");
}
