//! Typed instrument handles: every metric in [`crate::semconv`] built once from one [`Meter`].
//!
//! Call sites hold [`Instruments`] (or the global [`crate::instruments()`]) and record without
//! ever spelling a name, unit or bucket bound: those live in `semconv.rs`. Building the handles
//! once also means the OTel API resolves each instrument a single time instead of on every frame.
//!
//! Descriptions are the one thing not centralised in `semconv.rs` — they are prose for a human
//! reading a metrics explorer, not an identifier anything joins on, so they live next to the
//! handle they describe.

use crate::sc;
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter};

/// Build a float histogram with an explicit description, unit and bucket boundaries.
///
/// The boundaries are non-negotiable at the call site: OTel's default exponential buckets cannot
/// express "one 60 Hz frame" (16.7 ms), so a histogram without them cannot answer "how many
/// frames missed the budget".
fn hist(meter: &Meter, name: &'static str, desc: &'static str, unit: &'static str, bounds: Vec<f64>) -> Histogram<f64> {
    meter.f64_histogram(name).with_description(desc).with_unit(unit).with_boundaries(bounds).build()
}

/// Every P0 instrument, as one struct so a `Meter` is asked for each handle exactly once.
///
/// Field order follows the instrument table in `semconv.rs` (frame → interaction → raster/task →
/// texture → GPU/repaint → self-health → startup/footprint).
#[derive(Debug)]
pub struct Instruments {
    /// Wall time of one painted frame.
    pub frame_duration: Histogram<f64>,
    /// Wall time of one frame phase, discriminated by `phase`.
    pub frame_phase: Histogram<f64>,
    /// CPU time of one frame phase: `cpu_time / duration` separates work from waiting.
    pub frame_phase_cpu: Histogram<f64>,
    /// Frame-to-frame interval — the raw material of jitter and 1% low.
    pub frame_delta: Histogram<f64>,
    /// Frames that missed their budget, discriminated by `jank.class`.
    pub frame_janky: Counter<u64>,
    /// Signed pacing error against the display interval.
    pub pacing_error: Histogram<f64>,
    /// Tearing proxy: frame presented late.
    pub frame_late: Counter<u64>,
    /// Tearing proxy: same content presented twice.
    pub frame_duplicate: Counter<u64>,
    /// Tearing proxy: one update split across two presents.
    pub frame_split: Counter<u64>,
    /// Interaction to Next Paint.
    pub interaction_duration: Histogram<f64>,
    /// INP phase breakdown, discriminated by `inp.phase`.
    pub interaction_phase: Histogram<f64>,
    /// Interactions observed, discriminated by `interaction.type`.
    pub interaction_count: Counter<u64>,
    /// Pointer events merged into one frame — the collapsed-gesture false-negative guard.
    pub interaction_coalesced: Counter<u64>,
    /// Slide CPU raster wall time.
    pub raster_duration: Histogram<f64>,
    /// Rastered megapixels; normalises raster cost into `ms/Mpx`.
    pub raster_pixels: Histogram<f64>,
    /// Main-thread task duration, discriminated by `task.name`.
    pub task_duration: Histogram<f64>,
    /// Texture cache lookups, discriminated by `cache.result` and `cache.reason`.
    pub texture_cache: Counter<u64>,
    /// Repeated misses on the same slide inside the thrash window.
    pub texture_thrash: Counter<u64>,
    /// Bytes per texture upload.
    pub texture_bytes: Histogram<f64>,
    /// GPU frame time from timestamp queries.
    pub gpu_frame_time: Histogram<f64>,
    /// GPU utilisation, sampled — never per frame.
    pub gpu_utilization: Gauge<f64>,
    /// GPU memory in use — catches texture-cache leaks.
    pub gpu_memory_used: Gauge<f64>,
    /// `request_repaint*` calls, discriminated by `repaint.source`.
    pub repaint_requested: Counter<u64>,
    /// Records the pipeline itself dropped; non-zero invalidates the run.
    pub dropped_records: Counter<u64>,
    /// Cost of exporting telemetry — feeds the overhead guard.
    pub export_duration: Histogram<f64>,
    /// Time to first frame (FCP analogue).
    pub time_to_first_frame: Histogram<f64>,
    /// Time to the first full canvas paint (LCP analogue).
    pub largest_paint: Histogram<f64>,
    /// Resident set size.
    pub memory_rss: Gauge<f64>,
    /// Binary size, discriminated by `artifact`.
    pub binary_size: Gauge<f64>,
}

impl Instruments {
    /// Build every handle from `meter`.
    ///
    /// `meter` must come from the provider the app installed, before this call: the global API
    /// binds a `Meter` to the provider configured at that moment, so a meter obtained earlier
    /// stays no-op forever. The app calls [`crate::init_instruments()`] right after
    /// `set_meter_provider` for exactly this reason.
    pub fn new(meter: &Meter) -> Self {
        let f = sc::FRAME_BOUNDS_MS.to_vec();
        let i = sc::INP_BOUNDS_MS.to_vec();
        let px = sc::PIXEL_BOUNDS.to_vec();
        let by = sc::BYTE_BOUNDS.to_vec();
        Self {
            frame_duration: hist(meter, sc::M_FRAME_DURATION, "Frame wall time", sc::U_MS, f.clone()),
            frame_phase: hist(meter, sc::M_FRAME_PHASE, "Frame phase wall time", sc::U_MS, f.clone()),
            frame_phase_cpu: hist(meter, sc::M_FRAME_PHASE_CPU, "Frame phase CPU time", sc::U_MS, f.clone()),
            frame_delta: hist(meter, sc::M_FRAME_DELTA, "Frame-to-frame jitter", sc::U_MS, f.clone()),
            frame_janky: meter.u64_counter(sc::M_FRAME_JANKY).with_description("Janky frames").with_unit(sc::U_ONE).build(),
            pacing_error: hist(meter, sc::M_PACING_ERROR, "Frame pacing error vs vsync", sc::U_MS, f.clone()),
            frame_late: meter.u64_counter(sc::M_FRAME_LATE).with_unit(sc::U_ONE).build(),
            frame_duplicate: meter.u64_counter(sc::M_FRAME_DUPLICATE).with_unit(sc::U_ONE).build(),
            frame_split: meter.u64_counter(sc::M_FRAME_SPLIT).with_unit(sc::U_ONE).build(),
            interaction_duration: hist(meter, sc::M_INTERACTION_DURATION, "Interaction to Next Paint", sc::U_MS, i.clone()),
            interaction_phase: hist(meter, sc::M_INTERACTION_PHASE, "INP phase duration", sc::U_MS, i.clone()),
            interaction_count: meter.u64_counter(sc::M_INTERACTION_COUNT).with_unit(sc::U_ONE).build(),
            interaction_coalesced: meter.u64_counter(sc::M_INTERACTION_COALESCED).with_unit(sc::U_ONE).build(),
            raster_duration: hist(meter, sc::M_RASTER_DURATION, "Slide CPU raster duration", sc::U_MS, f.clone()),
            raster_pixels: hist(meter, sc::M_RASTER_PIXELS, "Rastered megapixels", sc::U_MPX, px),
            task_duration: hist(meter, sc::M_TASK_DURATION, "Main-thread task duration", sc::U_MS, f.clone()),
            texture_cache: meter.u64_counter(sc::M_CACHE).with_unit(sc::U_ONE).build(),
            texture_thrash: meter.u64_counter(sc::M_CACHE_THRASH).with_unit(sc::U_ONE).build(),
            texture_bytes: hist(meter, sc::M_TEXTURE_BYTES, "Texture bytes per upload", sc::U_BY, by),
            gpu_frame_time: hist(meter, sc::M_GPU_FRAME_TIME, "GPU frame time", sc::U_MS, f.clone()),
            gpu_utilization: meter.f64_gauge(sc::M_GPU_UTILIZATION).with_unit(sc::U_ONE).build(),
            gpu_memory_used: meter.f64_gauge(sc::M_GPU_MEMORY_USED).with_unit(sc::U_BY).build(),
            repaint_requested: meter.u64_counter(sc::M_REPAINT_REQUESTED).with_unit(sc::U_ONE).build(),
            dropped_records: meter.u64_counter(sc::M_DROPPED_RECORDS).with_unit(sc::U_ONE).build(),
            export_duration: hist(meter, sc::M_EXPORT_DURATION, "OTLP export duration", sc::U_MS, f.clone()),
            time_to_first_frame: hist(meter, sc::M_TTF_FIRST_FRAME, "Time to first frame", sc::U_MS, f.clone()),
            largest_paint: hist(meter, sc::M_LARGEST_PAINT, "Largest contentful paint", sc::U_MS, i),
            memory_rss: meter.f64_gauge(sc::M_MEMORY_RSS).with_unit(sc::U_BY).build(),
            binary_size: meter.f64_gauge(sc::M_BINARY_SIZE).with_unit(sc::U_BY).build(),
        }
    }
}
