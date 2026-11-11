# DeckCraft telemetry semantic conventions

**Status:** current · **OpenTelemetry API/SDK:** `=0.33.1` · **Semantic conventions:** 1.42.0
(the version `opentelemetry-semantic-conventions` 0.33.1 pins) · **Updated:** 2026-11-11

This document mirrors [`crates/telemetry/src/semconv.rs`](../crates/telemetry/src/semconv.rs), which
is the source of truth. `tests/semconv.rs` guards the constants; `xtask/src/otel_report.rs` reads
the names through `deckcraft_telemetry::sc`, so a rename cannot leave the report behind. If the two
disagree, the Rust file wins and this page is the bug.

## Why a project namespace

OpenTelemetry has no stable convention for UI frame rendering. Semconv 1.42.0 defines nothing for
frame time, jank, interaction latency, texture uploads or raster cost, so those instruments live
under **`deckcraft.`** — lowercase, dot-separated, no unit suffix in the name (the unit travels on
the instrument, in UCUM: `ms`, `By`, `1`, `Mpx`).

Keys that *do* have a standard name are imported, never redefined: `service.name`,
`service.version`, `deployment.environment.name`, `telemetry.sdk.*`. The hardware keys are
project-owned (`hw.gpu.adapter`, `hw.gpu.driver`, `renderer.backend`, `display.*`,
`device.pixel_ratio`) because the standard `hw.*` group is behind the unstable
`semconv_experimental` feature in 0.33.1; they move to `hw.model`/`hw.driver_version` when that
group graduates.

## Units (UCUM)

| Constant | Value | Used by |
|---|---|---|
| `U_MS` | `ms` | every duration |
| `U_BY` | `By` | bytes, memory |
| `U_ONE` | `1` | counts and ratios (never `count`) |
| `U_MPX` | `Mpx` | raster output; `ms/Mpx` is derived, not emitted |

## Budgets and bucket bounds

| Constant | Value | Why |
|---|---|---|
| `FRAME_BOUNDS_MS` | `4, 8, 16.7, 33.3, 50, 100, 200, 500, 1000` | splits at one and two 60 Hz frames, then a coarse tail; default exponential buckets cannot express 16.7 ms |
| `INP_BOUNDS_MS` | `8, 16.7, 33.3, 50, 100, 200, 500` | keeps the 16.7/33.3 frame edges Web Vitals' 200 ms granularity hides |
| `PIXEL_BOUNDS` | `0.1, 0.25, 0.5, 1, 2, 4, 8` | rasterised megapixels |
| `BYTE_BOUNDS` | `65536 … 268435456` | texture payloads |
| `FRAME_BUDGET_MS` | `16.7` | one 60 Hz frame; the internal budget |
| `LONG_TASK_MS` | `50` | a task over this is a Long Task (RAIL definition) |

## Instruments

One histogram with a `phase` attribute beats one instrument per phase. Every name below is a
`M_*` constant in `semconv.rs` and a field in `instruments.rs`.

| Instrument | Type | Unit | Attributes | Emitted today |
|---|---|---|---|---|
| `deckcraft.frame.duration` | Histogram f64 | ms | `frame.kind`, `cache.result` (reserved; recorded with no attributes) | yes, one per painted frame |
| `deckcraft.frame.phase.duration` | Histogram f64 | ms | `phase` | yes: build, raster, convert, upload, present |
| `deckcraft.frame.phase.cpu_time` | Histogram f64 | ms | `phase` | **no** — no thread-CPU clock without `unsafe`; `cpu/wall` is `n/a` |
| `deckcraft.frame.delta` | Histogram f64 | ms | — | yes; the interval between frame **starts** (frame pacing), not a duration delta |
| `deckcraft.frame.janky` | Counter u64 | `1` | `jank.class` | yes; exclusive classes, one per janky frame |
| `deckcraft.pacing.error` | Histogram f64 | ms | — | yes; `delta - display_interval` |
| `deckcraft.frame.late` | Counter u64 | `1` | — | yes; tearing proxy |
| `deckcraft.frame.duplicate` | Counter u64 | `1` | — | yes; tearing proxy, only reachable without vsync |
| `deckcraft.frame.split` | Counter u64 | `1` | — | yes; tearing proxy |
| `deckcraft.interaction.duration` | Histogram f64 | ms | `interaction.type` | yes; INP |
| `deckcraft.interaction.phase.duration` | Histogram f64 | ms | `inp.phase` | yes; input_delay, processing, presentation |
| `deckcraft.interaction.count` | Counter u64 | `1` | `interaction.type` | yes |
| `deckcraft.interaction.coalesced_events` | Counter u64 | `1` | `interaction.type` | yes; non-zero invalidates per-frame pointer pricing |
| `deckcraft.slide.raster.duration` | Histogram f64 | ms | `slide.index`, `render.scale`, `render.threads`, `render.second_pass` | yes |
| `deckcraft.slide.raster.pixels` | Histogram f64 | Mpx | `slide.index`, `render.scale` | yes |
| `deckcraft.task.duration` | Histogram f64 | ms | `task.name` | yes |
| `deckcraft.texture.cache` | Counter u64 | `1` | `cache.kind`, `cache.result`, `cache.reason` | yes |
| `deckcraft.texture.cache.thrash` | Counter u64 | `1` | `slide.index` | yes; the exact same cache key missing twice within 250 ms, not a per-frame edit |
| `deckcraft.texture.bytes` | Histogram f64 | By | `cache.kind`, `cache.result` | yes |
| `deckcraft.gpu.frame.time` | Histogram f64 | ms | — | **no** — no wgpu timestamp queries in the egui fork; a wall-clock upload labelled GPU time would be fabricated |
| `deckcraft.gpu.utilization` | Gauge f64 | `1` | — | only with `DECKCRAFT_GPU_METRICS=1` (nvidia-smi, 5 s cadence) |
| `deckcraft.gpu.memory.used` | Gauge f64 | By | — | only with `DECKCRAFT_GPU_METRICS=1` |
| `deckcraft.repaint.requested` | Counter u64 | `1` | `repaint.source` | yes |
| `deckcraft.telemetry.dropped_records` | Counter u64 | `1` | — | **no** — SDK 0.33.1 exposes no batch-processor drop counter; the report proves health by comparing span and histogram counts |
| `deckcraft.telemetry.export.duration` | Histogram f64 | ms | — | **no** — reserved for the overhead guard; the file exporters do not time themselves |
| `deckcraft.startup.time_to_first_frame` | Histogram f64 | ms | — | yes, once per process |
| `deckcraft.startup.time_to_interactive` | Histogram f64 | ms | — | yes, once per process (first accepted input) |
| `deckcraft.content.largest_paint` | Histogram f64 | ms | `content.type` | yes, once per process; **clock-origin relative**, an LCP analogue, not Web Vitals LCP |
| `deckcraft.memory.rss` | Gauge f64 | By | — | **no** — declared for the footprint sampler |
| `deckcraft.app.binary_size` | Gauge f64 | By | `artifact` | **no** — declared for the release job |

## Attribute keys

### Per frame and per interaction

| Key | Values / meaning |
|---|---|
| `frame.kind` | `interactive`, `idle`, `startup` |
| `frame.id` | monotonic frame counter; correlates a span with a log line |
| `frame.result` | `painted`, `skipped`, `errored` |
| `frame.budget_ms` | the budget the frame was judged against |
| `phase` | one of the phase values below |
| `jank.class` | `missed_vsync`, `dropped_frame`, `long_task` |
| `interaction.type` | `drag`, `click`, `key`, `text`, `ime` |
| `inp.phase` | `input_delay`, `processing`, `presentation` |
| `inp.input_delay_ms`, `inp.processing_ms`, `inp.presentation_ms`, `inp.total_ms` | the three INP phases and their sum |
| `slide.index`, `render.scale`, `render.threads`, `render.second_pass` | raster identity |
| `cache.kind` | `canvas` (a preview kind is reserved) |
| `cache.result` | `hit`, `miss` |
| `cache.reason` | `slide`, `size`, `region`, `edit`, `extra`, `editing_text`, `invalidate` |
| `texture.width`, `texture.height`, `texture.megapixels` | the normaliser that makes renderers comparable |
| `display.present_mode` | `fifo`, `immediate`, `mailbox` |
| `content.type` | `slide-canvas`, `preview` |
| `repaint.source` | `gif`, `anim_preview`, `control`, `continuous`, `synthetic` |
| `task.name` | `export.pdf`, `export`, `raster`, `font_load`, `open`, `adapter_select` |
| `deck.zoom` | the zoom the frame was rendered at |
| `artifact` | `native`, `wasm` |
| `tearing.proxy` | `late`, `split`, `duplicate` |

### Resource (process-wide)

| Key | Meaning |
|---|---|
| `service.name` | `deckcraft` (standard) |
| `service.version` | workspace version (standard) |
| `deployment.environment.name` | `test`, `dev`, `prod` (standard) |
| `build.profile` | `debug`, `release` |
| `hw.gpu.adapter` | e.g. `NVIDIA GeForce MX450`; `unknown` is never a guess |
| `hw.gpu.driver` | driver version |
| `renderer.backend` | `vello_cpu`, `wgpu/vulkan`, `wgpu/gl`, `wgpu/metal` |
| `renderer.device_type` | `discrete`, `integrated`, `software` |
| `display.vsync` | `on`, `off` |
| `display.subsystem` | `x11`, `wayland`, `win32`, `cocoa` |
| `display.refresh_hz` | the real refresh rate, not an assumed 60 |
| `device.pixel_ratio` | DPI scale; frames at different ratios are not comparable |
| `test.run_id` | groups one A/B run |
| `test.variant` | `baseline`, `candidate` |
| `test.zoom` | the fixed zoom an A/B run was recorded at |
| `test.gesture` | the fixed gesture an A/B run was recorded with |

## Phase values

`input_delay`, `build`, `raster`, `convert`, `upload`, `tessellate`, `present`, `processing`,
`presentation`.

`processing` and `presentation` are INP phases; the others are frame phases. `tessellate` is
declared for the GPU/text path and is not emitted by the CPU renderer today.

## Spans and events

| Name | Kind | Meaning |
|---|---|---|
| `deckcraft.frame` | Span | root of one painted frame; children are phases |
| `deckcraft.phase.<phase>` | Span | one frame phase, named after its `phase` attribute |
| `deckcraft.interaction` | Span | one interaction, carrying the `inp.*` attributes; may span frames |
| `deckcraft.startup` | Span | process start to first frame |
| `deckcraft.cache.miss` | Event | a texture cache miss, with `cache.reason` |
| `deckcraft.tearing.suspect` | Event | a pacing proxy fired; **not** proof of optical tearing |
| `deckcraft.long_task` | Event | a task exceeded `LONG_TASK_MS`, with `task.name` |

Exact per-frame samples come from `endTimeUnixNano - startTimeUnixNano` on the frame spans. Metric
histograms answer "how many frames missed the budget"; they cannot answer "what was the worst
frame". The report uses spans for the frame/INP percentiles and histograms for the phase table,
whose raster/convert/upload phases exist only as instruments.

## What is deliberately not emitted

- **`dropped_records`** — SDK 0.33.1 publishes no batch-processor drop counter. The report compares
  the frame-span count with the `frame.duration` histogram count instead; a shortfall is a dropped
  record.
- **`gpu.frame.time`** — no timestamp-query support in the pinned egui fork. `gpu.utilization` and
  `gpu.memory.used` come from nvidia-smi, on a 5 s timer, never per frame.
- **`frame.phase.cpu_time`** — a thread-CPU clock needs `unsafe` (forbidden) or a new dependency.
  Wall time labelled CPU time would make `cpu/wall` a fabrication; the report prints `n/a`.
- **`memory.rss`, `app.binary_size`** — declared for a later footprint/release sampler; a zero here
  would look like a measurement.

## Revision history

| Date | Change |
|---|---|
| 2026-11-11 | `deckcraft.texture.cache.thrash` defined on the whole cache key (document `Arc`, zoom, region, animation), not the slide index: a drag that edits every frame no longer counts as thrash. |
| 2026-11-10 | Initial conventions for the OTel 0.33.1 instrumentation: 30 instruments, 8 frame phases, frame/interaction/startup spans, the three diagnostic events. |
