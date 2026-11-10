//! DeckCraft's OpenTelemetry semantic conventions: every instrument name, unit, bucket bound,
//! attribute key and attribute value the instrumentation emits, in one place.
//!
//! # Why a project namespace
//!
//! OpenTelemetry has no stable semantic convention for UI frame rendering: semconv 1.42.0 (the
//! version this crate pins, see [`opentelemetry_semantic_conventions::SCHEMA_URL`]) defines nothing
//! for frame time, jank, interaction latency, texture uploads or raster cost. Those instruments
//! therefore live under `deckcraft.` — lowercase, dot-separated, no unit suffix in the name (the
//! unit travels on the instrument, in UCUM: `ms`, `By`, `1`, `Mpx`).
//!
//! Keys that *do* have a standard name are **imported, never redefined**: `service.name`,
//! `service.version`, `deployment.environment.name`, `telemetry.sdk.*` and the hardware keys.
//!
//! # Source of truth
//!
//! This module is authoritative. `docs/telemetry-semconv.md` mirrors it, `tests/semconv.rs` guards
//! it, and `instruments.rs` builds every handle from the constants here — no name is spelled at a
//! call site. Adding an instrument means adding its `M_*` to [`METRIC_NAMES`] in the same edit.

// ---------------------------------------------------------------------------------------------
// Standard keys (re-exported from opentelemetry-semantic-conventions 0.33.1, semconv 1.42.0)
// ---------------------------------------------------------------------------------------------

pub use opentelemetry_semantic_conventions::attribute::{
    DEPLOYMENT_ENVIRONMENT_NAME, SERVICE_NAME, SERVICE_VERSION, TELEMETRY_SDK_LANGUAGE, TELEMETRY_SDK_NAME, TELEMETRY_SDK_VERSION,
};

/// Value of the `service.name` resource attribute.
pub const SERVICE_NAME_VALUE: &str = "deckcraft";

/// `deployment.environment.name` value for a test run (deterministic A/B, CI).
pub const V_ENV_TEST: &str = "test";
/// `deployment.environment.name` value for a developer machine.
pub const V_ENV_DEV: &str = "dev";
/// `deployment.environment.name` value for a shipped build.
pub const V_ENV_PROD: &str = "prod";

// ---------------------------------------------------------------------------------------------
// Instrumentation scope
// ---------------------------------------------------------------------------------------------

/// `scope.name`: the instrumentation library, as OTel recommends (the crate, not the app).
pub const SCOPE_NAME: &str = "deckcraft-telemetry";

/// `scope.version`: lets a collector separate runs recorded with different conventions.
pub const SCOPE_VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------------------------
// Units (UCUM)
// ---------------------------------------------------------------------------------------------

/// Milliseconds — every duration in this crate.
pub const U_MS: &str = "ms";
/// Bytes.
pub const U_BY: &str = "By";
/// A dimensionless count; OTel spells it `1`, never `count`.
pub const U_ONE: &str = "1";
/// Megapixels, the normalising unit for raster cost (`ms/Mpx` is derived, not emitted).
pub const U_MPX: &str = "Mpx";

// ---------------------------------------------------------------------------------------------
// Bucket bounds and budgets
// ---------------------------------------------------------------------------------------------

/// Explicit bucket bounds for frame and phase durations, in ms.
///
/// Splits at one (16.7) and two (33.3) 60 Hz frames so a histogram answers "how many frames missed
/// the budget" without interpolation; the coarse tail (50 → 1000) separates a long task from a
/// stall. Linear or default exponential buckets cannot express the 60 Hz budget at all.
pub const FRAME_BOUNDS_MS: &[f64] = &[4.0, 8.0, 16.7, 33.3, 50.0, 100.0, 200.0, 500.0, 1000.0];

/// Explicit bucket bounds for interaction (INP) durations, in ms.
///
/// Starts at Web Vitals' "good" boundary (200 ms is the INP *threshold*, not a bucket edge here)
/// but, unlike Web Vitals, keeps the 16.7/33.3 frame edges: an editor must see the difference
/// between one and two frames of latency, which Web Vitals' 200 ms granularity hides.
pub const INP_BOUNDS_MS: &[f64] = &[8.0, 16.7, 33.3, 50.0, 100.0, 200.0, 500.0];

/// Explicit bucket bounds for rasterised work, in megapixels.
pub const PIXEL_BOUNDS: &[f64] = &[0.1, 0.25, 0.5, 1.0, 2.0, 4.0, 8.0];

/// Explicit bucket bounds for texture payloads, in bytes.
pub const BYTE_BOUNDS: &[f64] = &[65_536.0, 262_144.0, 1_048_576.0, 4_194_304.0, 16_777_216.0, 67_108_864.0, 268_435_456.0];

/// One frame at 60 Hz, in ms. The internal budget; Web Vitals thresholds are far looser.
pub const FRAME_BUDGET_MS: f64 = 16.7;

/// A task over this many ms is a Long Task (the web/RAIL definition this project reuses).
pub const LONG_TASK_MS: f64 = 50.0;

// ---------------------------------------------------------------------------------------------
// Instruments — metric names
// ---------------------------------------------------------------------------------------------

/// Wall time of one painted frame, ms. Attributes: `frame.kind`, `cache.result`.
pub const M_FRAME_DURATION: &str = "deckcraft.frame.duration";
/// Duration of one frame phase, ms. Attribute: `phase`.
pub const M_FRAME_PHASE: &str = "deckcraft.frame.phase.duration";
/// CPU time consumed by one frame phase, ms. `cpu_time / duration` separates work from waiting.
pub const M_FRAME_PHASE_CPU: &str = "deckcraft.frame.phase.cpu_time";
/// Frame-to-frame interval, ms — jitter, the raw material of 1% low.
pub const M_FRAME_DELTA: &str = "deckcraft.frame.delta";
/// Frames that missed their budget. Attribute: `jank.class`.
pub const M_FRAME_JANKY: &str = "deckcraft.frame.janky";
/// Signed pacing error against the display interval, ms.
pub const M_PACING_ERROR: &str = "deckcraft.pacing.error";
/// Tearing proxy: frames presented late.
pub const M_FRAME_LATE: &str = "deckcraft.frame.late";
/// Tearing proxy: the same content presented twice.
pub const M_FRAME_DUPLICATE: &str = "deckcraft.frame.duplicate";
/// Tearing proxy: one update split across two presents.
pub const M_FRAME_SPLIT: &str = "deckcraft.frame.split";
/// Interaction to Next Paint (INP), ms. Attribute: `interaction.type`.
pub const M_INTERACTION_DURATION: &str = "deckcraft.interaction.duration";
/// INP phase breakdown, ms. Attribute: `inp.phase`.
pub const M_INTERACTION_PHASE: &str = "deckcraft.interaction.phase.duration";
/// Interactions observed. Attribute: `interaction.type`.
pub const M_INTERACTION_COUNT: &str = "deckcraft.interaction.count";
/// Pointer events merged into one frame — the collapsed-gesture false-negative guard.
pub const M_INTERACTION_COALESCED: &str = "deckcraft.interaction.coalesced_events";
/// Slide raster wall time, ms. Attributes: `slide.index`, `render.scale`, `render.threads`, `render.second_pass`.
pub const M_RASTER_DURATION: &str = "deckcraft.slide.raster.duration";
/// Slide raster output size, Mpx. Attributes: `slide.index`, `render.scale`.
pub const M_RASTER_PIXELS: &str = "deckcraft.slide.raster.pixels";
/// Main-thread task duration, ms. Attribute: `task.name`.
pub const M_TASK_DURATION: &str = "deckcraft.task.duration";
/// Texture cache lookups. Attributes: `cache.kind`, `cache.result`, `cache.reason`.
pub const M_CACHE: &str = "deckcraft.texture.cache";
/// Repeated misses on the same slide inside the thrash window.
pub const M_CACHE_THRASH: &str = "deckcraft.texture.cache.thrash";
/// Bytes uploaded per texture, `By`. Attributes: `cache.kind`, `cache.result`.
pub const M_TEXTURE_BYTES: &str = "deckcraft.texture.bytes";
/// GPU frame time from timestamp queries, ms.
pub const M_GPU_FRAME_TIME: &str = "deckcraft.gpu.frame.time";
/// GPU utilisation, `1` (0–1 or percent, fixed by the exporter and documented with it).
pub const M_GPU_UTILIZATION: &str = "deckcraft.gpu.utilization";
/// GPU memory in use, `By` — catches texture-cache leaks.
pub const M_GPU_MEMORY_USED: &str = "deckcraft.gpu.memory.used";
/// `request_repaint*` calls. Attribute: `repaint.source`.
pub const M_REPAINT_REQUESTED: &str = "deckcraft.repaint.requested";
/// Records the telemetry pipeline itself dropped. Non-zero invalidates the measurement.
pub const M_DROPPED_RECORDS: &str = "deckcraft.telemetry.dropped_records";
/// Cost of exporting telemetry, ms — feeds the overhead guard.
pub const M_EXPORT_DURATION: &str = "deckcraft.telemetry.export.duration";
/// Time to first frame, ms (FCP analogue).
pub const M_TTF_FIRST_FRAME: &str = "deckcraft.startup.time_to_first_frame";
/// Time to the first full canvas paint, ms (LCP analogue). Attribute: `content.type`.
pub const M_LARGEST_PAINT: &str = "deckcraft.content.largest_paint";
/// Resident set size, `By`.
pub const M_MEMORY_RSS: &str = "deckcraft.memory.rss";
/// Binary size, `By`. Attribute: `artifact`.
pub const M_BINARY_SIZE: &str = "deckcraft.app.binary_size";

/// Every metric name this crate declares, in instrument-table order.
///
/// `tests/semconv.rs` checks that all of them are namespaced under `deckcraft.` and unique. A new
/// `M_*` constant must be added here in the same edit — nothing else can see an unregistered name.
pub const METRIC_NAMES: &[&str] = &[
    M_FRAME_DURATION,
    M_FRAME_PHASE,
    M_FRAME_PHASE_CPU,
    M_FRAME_DELTA,
    M_FRAME_JANKY,
    M_PACING_ERROR,
    M_FRAME_LATE,
    M_FRAME_DUPLICATE,
    M_FRAME_SPLIT,
    M_INTERACTION_DURATION,
    M_INTERACTION_PHASE,
    M_INTERACTION_COUNT,
    M_INTERACTION_COALESCED,
    M_RASTER_DURATION,
    M_RASTER_PIXELS,
    M_TASK_DURATION,
    M_CACHE,
    M_CACHE_THRASH,
    M_TEXTURE_BYTES,
    M_GPU_FRAME_TIME,
    M_GPU_UTILIZATION,
    M_GPU_MEMORY_USED,
    M_REPAINT_REQUESTED,
    M_DROPPED_RECORDS,
    M_EXPORT_DURATION,
    M_TTF_FIRST_FRAME,
    M_LARGEST_PAINT,
    M_MEMORY_RSS,
    M_BINARY_SIZE,
];

// ---------------------------------------------------------------------------------------------
// Attribute keys — per-frame and per-interaction
// ---------------------------------------------------------------------------------------------

/// `frame.kind` — interactive, idle, startup.
pub const A_FRAME_KIND: &str = "frame.kind";
/// `frame.id` — monotonic frame counter, correlates a span with a log line.
pub const A_FRAME_ID: &str = "frame.id";
/// `frame.result` — painted, skipped, errored.
pub const A_FRAME_RESULT: &str = "frame.result";
/// `frame.budget_ms` — the budget the frame was judged against.
pub const A_FRAME_BUDGET_MS: &str = "frame.budget_ms";
/// `phase` — one of the [`P_BUILD`]-style values; the phase-histogram discriminator.
pub const A_PHASE: &str = "phase";
/// `jank.class` — one of the `V_JANK_*` values.
pub const A_JANK_CLASS: &str = "jank.class";
/// `interaction.type` — drag, click, key, text.
pub const A_INTERACTION_TYPE: &str = "interaction.type";
/// `inp.phase` — one of [`P_INPUT_DELAY`], [`P_PROCESSING`], [`P_PRESENTATION`].
pub const A_INP_PHASE: &str = "inp.phase";
/// `inp.input_delay_ms` — event to handler start.
pub const A_INP_INPUT_DELAY_MS: &str = "inp.input_delay_ms";
/// `inp.processing_ms` — handler start to frame request.
pub const A_INP_PROCESSING_MS: &str = "inp.processing_ms";
/// `inp.presentation_ms` — frame request to pixels presented.
pub const A_INP_PRESENTATION_MS: &str = "inp.presentation_ms";
/// `inp.total_ms` — event to presented pixels; the INP number itself.
pub const A_INP_TOTAL_MS: &str = "inp.total_ms";
/// `slide.index`.
pub const A_SLIDE_INDEX: &str = "slide.index";
/// `render.scale` — raster scale factor.
pub const A_RENDER_SCALE: &str = "render.scale";
/// `render.threads` — threads the raster actually used.
pub const A_RENDER_THREADS: &str = "render.threads";
/// `render.second_pass` — whether this was the region re-render after the first pass.
pub const A_RENDER_SECOND_PASS: &str = "render.second_pass";
/// `cache.kind` — canvas, or a preview kind.
pub const A_CACHE_KIND: &str = "cache.kind";
/// `cache.result` — [`V_HIT`] or [`V_MISS`].
pub const A_CACHE_RESULT: &str = "cache.result";
/// `cache.reason` — which key field invalidated the entry; one of the `V_REASON_*` values.
pub const A_CACHE_REASON: &str = "cache.reason";
/// `texture.width` — pixels.
pub const A_TEXTURE_WIDTH: &str = "texture.width";
/// `texture.height` — pixels.
pub const A_TEXTURE_HEIGHT: &str = "texture.height";
/// `texture.megapixels` — the normaliser that makes renderers comparable.
pub const A_TEXTURE_MEGAPIXELS: &str = "texture.megapixels";
/// `display.present_mode` — fifo, immediate, mailbox.
pub const A_DISPLAY_PRESENT_MODE: &str = "display.present_mode";
/// `content.type` — slide-canvas, preview.
pub const A_CONTENT_TYPE: &str = "content.type";
/// `repaint.source` — one of the `V_REPAINT_*` values.
pub const A_REPAINT_SOURCE: &str = "repaint.source";
/// `task.name` — one of the `V_TASK_*` values.
pub const A_TASK_NAME: &str = "task.name";
/// `deck.zoom` — the zoom the frame was rendered at.
pub const A_DECK_ZOOM: &str = "deck.zoom";
/// `artifact` — [`V_ARTIFACT_NATIVE`] or [`V_ARTIFACT_WASM`].
pub const A_ARTIFACT: &str = "artifact";

// ---------------------------------------------------------------------------------------------
// Attribute keys — resource (process-wide)
// ---------------------------------------------------------------------------------------------

/// `build.profile` — debug or release. No stable semconv exists for it.
pub const A_BUILD_PROFILE: &str = "build.profile";
/// GPU adapter name, e.g. `NVIDIA GeForce MX450`.
///
/// The nearest standard key is `hw.model`, but in `opentelemetry-semantic-conventions` 0.33.1 the
/// whole `hw.*` group sits behind the unstable `semconv_experimental` feature, and an L0 crate does
/// not opt into an unstable convention. The key stays project-owned here and moves to `hw.model`
/// when that group graduates.
pub const A_HW_GPU_ADAPTER: &str = "hw.gpu.adapter";
/// GPU driver version — project-owned for the same reason as [`A_HW_GPU_ADAPTER`]
/// (the standard counterpart is `hw.driver_version`).
pub const A_HW_GPU_DRIVER: &str = "hw.gpu.driver";
/// `renderer.backend` — vello_cpu, wgpu/vulkan, wgpu/gl, wgpu/metal.
pub const A_RENDERER_BACKEND: &str = "renderer.backend";
/// `renderer.device_type` — discrete, integrated, software.
pub const A_RENDERER_DEVICE_TYPE: &str = "renderer.device_type";
/// `display.vsync` — on or off.
pub const A_DISPLAY_VSYNC: &str = "display.vsync";
/// `display.subsystem` — x11, wayland, win32, cocoa.
pub const A_DISPLAY_SUBSYSTEM: &str = "display.subsystem";
/// `display.refresh_hz` — the real refresh rate, not the assumed 60.
pub const A_DISPLAY_REFRESH_HZ: &str = "display.refresh_hz";
/// `device.pixel_ratio` — DPI scale; frames at different ratios are not comparable.
pub const A_DEVICE_PIXEL_RATIO: &str = "device.pixel_ratio";
/// `test.run_id` — groups one A/B run.
pub const A_TEST_RUN_ID: &str = "test.run_id";
/// `test.variant` — [`V_BASELINE`] or [`V_CANDIDATE`].
pub const A_TEST_VARIANT: &str = "test.variant";
/// `test.zoom` — the fixed zoom an A/B run was recorded at.
pub const A_TEST_ZOOM: &str = "test.zoom";
/// `test.gesture` — the fixed gesture an A/B run was recorded with.
pub const A_TEST_GESTURE: &str = "test.gesture";

// ---------------------------------------------------------------------------------------------
// Attribute values
// ---------------------------------------------------------------------------------------------

/// `cache.result`: the entry was reused.
pub const V_HIT: &str = "hit";
/// `cache.result`: the entry was rebuilt.
pub const V_MISS: &str = "miss";
/// `cache.kind`: the slide canvas texture.
pub const V_CANVAS: &str = "canvas";
/// `content.type`: the slide canvas paint.
pub const V_CONTENT_SLIDE_CANVAS: &str = "slide-canvas";
/// `test.variant`: the unchanged build.
pub const V_BASELINE: &str = "baseline";
/// `test.variant`: the changed build.
pub const V_CANDIDATE: &str = "candidate";
/// `artifact`: the native binary.
pub const V_ARTIFACT_NATIVE: &str = "native";
/// `artifact`: the wasm module.
pub const V_ARTIFACT_WASM: &str = "wasm";
/// `jank.class`: the frame arrived after its vsync deadline.
pub const V_JANK_MISSED_VSYNC: &str = "missed_vsync";
/// `jank.class`: the frame was not presented at all.
pub const V_JANK_DROPPED_FRAME: &str = "dropped_frame";
/// `jank.class`: one task blocked the main thread over [`LONG_TASK_MS`].
pub const V_JANK_LONG_TASK: &str = "long_task";
/// `cache.reason`: a different slide.
pub const V_REASON_SLIDE: &str = "slide";
/// `cache.reason`: a different render size.
pub const V_REASON_SIZE: &str = "size";
/// `cache.reason`: a different dirty region.
pub const V_REASON_REGION: &str = "region";
/// `cache.reason`: the edit counter moved.
pub const V_REASON_EDIT: &str = "edit";
/// `cache.reason`: extra render flags changed.
pub const V_REASON_EXTRA: &str = "extra";
/// `cache.reason`: the text-edit overlay state changed.
pub const V_REASON_EDITING_TEXT: &str = "editing_text";
/// `cache.reason`: explicitly invalidated.
pub const V_REASON_INVALIDATE: &str = "invalidate";
/// `repaint.source`: an animated GIF elsewhere in the deck.
pub const V_REPAINT_GIF: &str = "gif";
/// `repaint.source`: an animation preview.
pub const V_REPAINT_ANIM_PREVIEW: &str = "anim_preview";
/// `repaint.source`: the JSON-lines control channel.
pub const V_REPAINT_CONTROL: &str = "control";
/// `repaint.source`: the continuous paint loop.
pub const V_REPAINT_CONTINUOUS: &str = "continuous";
/// `repaint.source`: a synthetic benchmark driver.
pub const V_REPAINT_SYNTHETIC: &str = "synthetic";
/// `interaction.type`: the pointer pressed and moved before releasing.
pub const V_INTERACTION_DRAG: &str = "drag";
/// `interaction.type`: a pointer press and release without a move in between.
pub const V_INTERACTION_CLICK: &str = "click";
/// `interaction.type`: a key press reached the app.
pub const V_INTERACTION_KEY: &str = "key";
/// `interaction.type`: a text input event (a control-channel `ui.text`).
pub const V_INTERACTION_TEXT: &str = "text";
/// `interaction.type`: an input-method event.
pub const V_INTERACTION_IME: &str = "ime";
/// `task.name`: PDF export.
pub const V_TASK_EXPORT_PDF: &str = "export.pdf";
/// `task.name`: a raster pass.
pub const V_TASK_RASTER: &str = "raster";
/// `task.name`: font loading.
pub const V_TASK_FONT_LOAD: &str = "font_load";
/// `task.name`: opening a deck.
pub const V_TASK_OPEN: &str = "open";
/// `task.name`: enumerating GPU adapters at startup.
pub const V_TASK_ADAPTER_SELECT: &str = "adapter_select";

// ---------------------------------------------------------------------------------------------
// Phase values — one histogram, one `phase` attribute, never one instrument per phase
// ---------------------------------------------------------------------------------------------

/// Time the input waited before the frame started.
pub const P_INPUT_DELAY: &str = "input_delay";
/// Layout and UI build.
pub const P_BUILD: &str = "build";
/// CPU rasterisation of the slide (vello_cpu).
pub const P_RASTER: &str = "raster";
/// Raster output to RGBA.
pub const P_CONVERT: &str = "convert";
/// Texture upload to the GPU.
pub const P_UPLOAD: &str = "upload";
/// Shape tessellation.
pub const P_TESSELLATE: &str = "tessellate";
/// Blit and present.
pub const P_PRESENT: &str = "present";
/// INP: event to handler start.
pub const P_PROCESSING: &str = "processing";
/// INP: pixels presented.
pub const P_PRESENTATION: &str = "presentation";

// ---------------------------------------------------------------------------------------------
// Spans and events
// ---------------------------------------------------------------------------------------------

/// Root span of one painted frame.
pub const S_FRAME: &str = "deckcraft.frame";
/// Span of one interaction, carrying the `inp.*` attributes.
pub const S_INTERACTION: &str = "deckcraft.interaction";
/// Span of startup, from process start to the first frame.
pub const S_STARTUP: &str = "deckcraft.startup";
/// Child spans of [`S_FRAME`] are `deckcraft.phase.<phase>`.
pub const S_PHASE_PREFIX: &str = "deckcraft.phase.";
/// Span event: a texture cache miss, with `cache.reason`.
pub const E_CACHE_MISS: &str = "deckcraft.cache.miss";
/// Span event: a frame showed a tearing symptom. A proxy — frame timestamps cannot prove optics.
pub const E_TEARING_SUSPECT: &str = "deckcraft.tearing.suspect";
/// Span event: a task exceeded [`LONG_TASK_MS`], with `task.name`.
pub const E_LONG_TASK: &str = "deckcraft.long_task";
