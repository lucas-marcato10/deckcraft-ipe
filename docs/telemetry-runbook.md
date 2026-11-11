# DeckCraft telemetry runbook

**Updated:** 2026-11-11 · **Pipeline:** OpenTelemetry 0.33.1, SDK at the app boundary, API-only in
the libraries.

Telemetry is **off by default**. Every instrument is a no-op until the application installs a
provider, so a normal launch pays nothing. This page is the operator's side: how to turn it on,
where the bytes go, how to read them, and what the numbers cannot prove.

## 1. Turn it on

| Variable | Meaning |
|---|---|
| `DECKCRAFT_OTEL` | Presence enables the SDK at launch. Any value works; `1` is conventional. |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | **Primary sink.** OTLP/HTTP protobuf endpoint, e.g. `http://127.0.0.1:4318`. The SDK also reads `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `_METRICS_ENDPOINT` and `_LOGS_ENDPOINT`. The HTTP exporter is built only when one of these is set. |
| `DECKCRAFT_OTEL_DIR` | **Secondary sink.** Directory for the OTLP/JSON Lines file exporter: `traces.jsonl`, `metrics.jsonl`, `logs.jsonl`, one signal per file. Used by CI and deterministic A/B runs. |
| `DECKCRAFT_GPU_METRICS` | Presence starts the nvidia-smi sampler (`gpu.utilization`, `gpu.memory.used`) on a 5 s timer. Never per frame. |

The app logs a warning and stays up if `DECKCRAFT_OTEL` is set with neither an endpoint nor a
directory: a run with no sink is a mistake, not a crash.

### Run identity

These become `resource` attributes and are what makes two runs comparable — or not:

`DECKCRAFT_ENV` (`test`/`dev`/`prod`), `DECKCRAFT_TEST_RUN_ID`, `DECKCRAFT_TEST_VARIANT`
(`baseline`/`candidate`), `DECKCRAFT_TEST_ZOOM`, `DECKCRAFT_GPU_ADAPTER`, `DECKCRAFT_RENDERER`,
`DECKCRAFT_DISPLAY_VSYNC`, `DECKCRAFT_DISPLAY_SUBSYSTEM`, `DECKCRAFT_DISPLAY_REFRESH_HZ`,
`DECKCRAFT_DEVICE_PIXEL_RATIO`, `DECKCRAFT_PRESENT_MODE`.

An unknown hardware value is absent or `unknown`, never a guess. The report refuses an A/B whose
comparable attributes differ (GPU adapter, renderer backend, DPI, display subsystem/refresh/vsync,
build profile, test zoom).

## 2. The collector is the primary path

Point `OTEL_EXPORTER_OTLP_ENDPOINT` at a collector with an `otlp` receiver. The SDK's own
`opentelemetry-otlp` exporter (protobuf over HTTP, blocking client on the SDK's own threads)
handles batching, retries and temporality; the collector handles aggregation, Views, sampling and
routing. Do **not** replace this with a `curl` loop that posts JSON lines — that throws away the
SDK's aggregation and the collector's interoperability.

For a file-based run, the same collector can tail the directory:

```yaml
receivers:
  otlp:
    protocols:
      http:
        endpoint: 127.0.0.1:4318
  otlpjsonfile:
    include:
      - /path/to/otel/*.jsonl
    # The default is 1 MiB per line and it TRUNCATES a longer one. A 60 fps frame burst can exceed
    # that; set this above the largest batch you expect (the file exporter writes one batch per
    # line, not one record per line).
    max_log_size: 16_777_216
```

`otlpjsonfile` is a **receiver**, so it reads the files the app writes; the app never talks to it
directly. The `max_log_size` caveat is the receiver's, not the app's: a truncated line is a lost
batch, and the report will show it as a shortfall (`dropped_records > 0`).

## 3. The report

```bash
cargo xtask otel-report <dir> [--baseline <dir>]
```

`<dir>` is the candidate; `--baseline` is the run it is compared with. Missing files are an empty
signal, not an error; an empty directory prints `frames: 0` and exits 0.

```
DeckCraft OTLP report   service=deckcraft 0.15.0  variant=candidate  run=otel-ab-1000
frames: 1021   p50=11.70ms p90=14.18ms p99=16.91ms worst=25.04ms  1%low_fps=53.6  CV=0.149
jank:   missed_vsync=14 dropped_frame=0 long_task=0  jank=14 longest_run=1
cache:  hit=18 miss=1003  hit_rate=1.8%  renders/interaction=1.00  thrash=0
INP (drag): p50=11.78ms p90=14.23ms p99=16.97ms worst=19.42ms
  input_delay=0.01ms  processing=0.03ms  presentation=11.74ms  coalesced=0
bottleneck (p99 by phase):        cpu/wall
  present       16.42 ms   38.2%       n/a
  ...
A/B (candidate/baseline): p50 1.00x  p99 0.97x  INP 0.98x  renders/interaction 1.00x
PASS: p99 within gate (<=1.10x), median within gate (<=1.05x)
```

How to read it:

- **`frames`, `p50/p90/p99/worst`** come from the exact `deckcraft.frame` span durations
  (`endTimeUnixNano - startTimeUnixNano`), nearest-rank. `1%low_fps` is `1000 / mean(worst 1%)`.
- **`CV`** is the sample coefficient of variation of the frame durations. The **gate** uses the
  relative standard error of the median, estimated by a deterministic bootstrap (256 resamples):
  with one directory there is one median, so the honest single-run analogue is how much it would
  move under resampling. Above 3% with ≥1,000 frames the report says *rerun on a quieter host*.
- **`jank`** counts frames by exclusive class; `jank=` is the number of **jank episodes** (maximal
  consecutive runs over 16.7 ms) and `longest_run` is the longest such run.
- **`cache`** is the texture cache counter; `renders/interaction` is raster passes per interaction.
  `thrash` counts the **exact same cache key** missing twice within 250 ms — the cache dropping a
  texture it just built. A drag that edits every frame is a new document per frame (a new `Arc`,
  so a new key) and is a legitimate re-render: it raises `miss`, never `thrash`. A non-zero
  `thrash` points at a cache that does not hold its own result (or at `invalidate()` between two
  identical requests), not at the edit rate.
- **`INP`** is the interaction span's `inp.total_ms`; the phase breakdown is the p50 of each
  `inp.*` attribute. `coalesced` must be 0 for a per-frame price.
- **`bottleneck`** is the `frame.phase.duration` histogram, p99 by `phase`. Raster/convert/upload
  have no span, so this table is histogram-derived (interpolated); the frame percentiles above stay
  exact. `cpu/wall` is `n/a`: no thread-CPU clock is available without `unsafe`.
- **`raster efficiency`** is `raster ms / raster Mpx`; `Mpx/s` is the same ratio inverted. `upload
  GB/s` divides texture bytes by the **queueing** call's wall time, not the GPU copy — treat it as
  an upper bound, not bandwidth.
- **`gpu`** is `n/a` unless `DECKCRAFT_GPU_METRICS` ran; `gpu.frame.time` needs timestamp queries
  the egui fork does not expose. `repaints/s idle` is `repaint.source=continuous` per second.
- **`tearing proxies`** are pacing heuristics (`late`/`split`/`duplicate`), never proof that the
  user saw a torn image. `pacing_error_p99` is histogram-interpolated.
- **`telemetry health`** compares the frame-span count with the `frame.duration` histogram count.
  `dropped_records > 0` invalidates the run; `export_p99` is `n/a` because export timing is not
  emitted.

### Gates

| Condition | Exit |
|---|---|
| `p99` ratio > 1.10× | non-zero, `FAIL: p99 …` |
| median ratio > 1.05× | non-zero, `FAIL: median …` |
| `dropped_records > 0` | non-zero, `FAIL: dropped_records …` |
| median CV > 3% with ≥1,000 frames | non-zero, `FAIL: median CV … — rerun on a quieter host` |
| comparable resource attributes differ | non-zero, `REFUSED: resource mismatch (…)` |

The gates apply only when a `--baseline` is given (except `dropped_records` and the CV, which are
properties of the candidate alone). Without a baseline the report prints `PASS: no baseline, gates
not applied`.

## 4. Three diagnostic queries

The metric names are translated by the collector/Prometheus exporter: dots become underscores, a
counter gains `_total`, a histogram gains `_bucket`/`_count`/`_sum`.

**Which phase is the bottleneck (p99 by phase):**

```promql
histogram_quantile(0.99, sum by (le, phase) (rate(deckcraft_frame_phase_duration_bucket[5m])))
```

**Interaction latency (INP p99 by type):**

```promql
histogram_quantile(0.99, sum by (le, interaction_type) (rate(deckcraft_interaction_duration_bucket[5m])))
```

**Jank ratio (janky frames per painted frame):**

```promql
sum(rate(deckcraft_frame_janky_total[5m])) / sum(rate(deckcraft_frame_duration_count[5m]))
```

Two more that answer the questions the frame table cannot:

```promql
# Renders per interaction: raster passes / interactions.
sum(rate(deckcraft_slide_raster_duration_count[5m])) / sum(rate(deckcraft_interaction_count_total[5m]))

# Idle repaint pressure: continuous repaints per second.
sum(rate(deckcraft_repaint_requested_total{repaint_source="continuous"}[5m]))
```

## 5. Paired A/B

The harness is `crates/ui-egui/tests/otel_ab.rs`: 10 warmups, **1,000 measured frames**, one
`ui.pointer` event per frame (so `coalesced` is 0), a fixed 0.1 pt drag at 300% zoom, one document
mutation per frame asserted via `revision` and `dirty`. It writes official OTLP JSON Lines through
the SDK's own proto transforms.

```bash
# Candidate and baseline are the same build here: this is the null A/B that proves the gate and
# the instrumentation's own overhead. Run them back to back on the same host, same DPI/renderer.
DECKCRAFT_OTEL_AB=1 DECKCRAFT_TEST_VARIANT=candidate DECKCRAFT_OTEL_DIR=target/otel/candidate \
  cargo test -p deckcraft-ui-egui --test otel_ab -- --ignored --nocapture

DECKCRAFT_OTEL_AB=1 DECKCRAFT_TEST_VARIANT=baseline DECKCRAFT_OTEL_DIR=target/otel/baseline \
  cargo test -p deckcraft-ui-egui --test otel_ab -- --ignored --nocapture

cargo xtask otel-report target/otel/candidate --baseline target/otel/baseline
```

The harness's display interval defaults to 60 Hz; set `DECKCRAFT_DISPLAY_REFRESH_HZ` (the real
variable) if the display is not 60 Hz. `DECKCRAFT_OTEL_AB_FRAMES=100` gives a quick smoke run — it
is **not** the gate, which requires 1,000.

The overhead guard is separate:

```bash
cargo test -p deckcraft-telemetry --test overhead
```

It checks two things: 10,000 records against the no-op provider finish under 100 ms, and a paired
on/off run of a synthetic 1 ms workload keeps the median within ±5%.

## 6. What the numbers cannot prove

- **Tearing.** `late`/`split`/`duplicate` are frame-timestamp heuristics. Optical proof needs
  Wayland `wp_presentation` feedback or an external capture; neither is available here.
- **GPU vs CPU attribution.** There is no GPU timestamp query, so `gpu.frame.time` is `n/a` and a
  slow frame cannot be blamed on the GPU. `gpu.utilization` is a 5 s nvidia-smi sample.
- **CPU/wall per phase.** `frame.phase.cpu_time` is not emitted; the report prints `n/a` instead of
  substituting wall time. A multi-threaded raster's CPU time belongs to worker threads this crate
  cannot clock without `unsafe`.
- **Perceived smoothness.** Frame timestamps approximate what the compositor did, not what the
  panel showed.
- **LCP.** `content.largest_paint` is milliseconds since the monotonic clock origin, recorded on the
  first canvas upload. It is an LCP analogue, not Web Vitals LCP (which counts from navigation).
- **Histogram quantiles.** The phase table, `pacing_error_p99` and `export_p99` are interpolated
  inside a bucket. The frame and INP percentiles are exact span samples.

## 7. Non-goals

This repository does not ship a collector, a dashboard or a CI performance service. The file
exporter and `otel-report` exist so a developer or a CI job can answer "which phase is slow" and
"did the change help" without one. Optimising the overlay/texture cache is a separate task; this
pipeline only makes its result provable.
