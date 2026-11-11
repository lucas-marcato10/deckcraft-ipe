//! The application boundary for OpenTelemetry: providers, exporters and the `log` bridge.
//!
//! Everything below this module emits into the global OTel API; this is the one place that
//! installs a real SDK, and the only place allowed to know an exporter exists. Telemetry is **off
//! by default**: [`init`] runs only when `DECKCRAFT_OTEL` is set, and until then every instrument
//! in `crates/telemetry` records into the API's no-op provider at no cost.
//!
//! # Two sinks, one pipeline
//!
//! * **Primary — OTLP/HTTP.** The SDK's own `opentelemetry-otlp` exporter, protobuf over HTTP,
//!   pointed by `OTEL_EXPORTER_OTLP_ENDPOINT` (the SDK reads `OTEL_EXPORTER_OTLP_*` itself; with
//!   none set the OTLP default of `http://localhost:4318` applies). Its default
//!   `reqwest-blocking-client` is what makes `PeriodicReader`/`BatchSpanProcessor` work from their
//!   own threads without a Tokio runtime — the async clients are the optional ones.
//! * **Secondary — OTLP/JSON Lines**, only when `DECKCRAFT_OTEL_DIR` is set: one file per signal
//!   (`traces.jsonl`, `metrics.jsonl`, `logs.jsonl`, per the OTLP File Exporter spec) written by
//!   [`crate::telemetry_file_exporter`]. This is what a CI job or a deterministic A/B run reads.
//!
//! When pointing a collector's `otlpjsonfilereceiver` at that directory, set `max_log_size` above
//! the largest expected batch: its default truncates a line at 1 MiB, and a frame burst can be
//! bigger than that.
//!
//! # Ordering, twice
//!
//! 1. `global::set_meter_provider(..)` **before** `deckcraft_telemetry::init_instruments()`: a
//!    `Meter` binds to the provider current when it is created, and every instrument handle is
//!    built from that `Meter`. Bind them to the no-op provider and metrics are dropped for the
//!    life of the process — with no compiler error.
//! 2. `global::set_tracer_provider(..)` before the first frame: the span helpers in
//!    `crates/telemetry` ask for the global tracer on every call (so they survive a late
//!    provider), but nothing is exported until the provider is installed.

use std::path::PathBuf;

use opentelemetry::logs::{Logger, LoggerProvider};
use opentelemetry::{KeyValue, global};
use opentelemetry_appender_log::OpenTelemetryLogBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;

use deckcraft_telemetry::semconv as sc;

use crate::logging::{AppLogger, Tee};
use crate::telemetry_file_exporter::{FileLogExporter, FileMetricExporter, FileSpanExporter};

/// Value for a hardware attribute nobody supplied: never guess a GPU or a backend.
const UNKNOWN: &str = "unknown";

/// One file per signal, as the OTLP File Exporter spec requires.
const TRACES_FILE: &str = "traces.jsonl";
const METRICS_FILE: &str = "metrics.jsonl";
const LOGS_FILE: &str = "logs.jsonl";

/// What identifies this run in every exported record.
///
/// Kept separate from the environment so the resource can be built and asserted without touching
/// process state: [`RunIdentity::from_env`] is the only impure part.
#[derive(Debug, Clone, PartialEq)]
struct RunIdentity {
    environment: String,
    run_id: String,
    variant: String,
    zoom: f64,
    gpu_adapter: String,
    renderer_backend: String,
    /// Present only when the display is actually known; a `1.0` guessed here would poison every
    /// cross-run comparison the A/B harness makes.
    display_vsync: Option<String>,
    display_subsystem: Option<String>,
    display_refresh_hz: Option<f64>,
    device_pixel_ratio: Option<f64>,
}

impl RunIdentity {
    /// Read the run identity from the environment, defaulting every string to a truthful value.
    ///
    /// The GPU adapter and the renderer backend are reported by the `egui_wgpu` startup record the
    /// log bridge carries; here they are `unknown` unless the harness says otherwise, because a
    /// wrong `hw.gpu.adapter` makes two different machines look comparable.
    fn from_env() -> RunIdentity {
        RunIdentity {
            environment: env_or("DECKCRAFT_ENV", sc::V_ENV_DEV),
            run_id: env_or("DECKCRAFT_TEST_RUN_ID", "adhoc"),
            variant: env_or("DECKCRAFT_TEST_VARIANT", "baseline"),
            zoom: env_f64("DECKCRAFT_TEST_ZOOM").unwrap_or(1.0),
            gpu_adapter: env_or("DECKCRAFT_GPU_ADAPTER", UNKNOWN),
            renderer_backend: env_or("DECKCRAFT_RENDERER", UNKNOWN),
            display_vsync: env_opt("DECKCRAFT_DISPLAY_VSYNC"),
            display_subsystem: env_opt("DECKCRAFT_DISPLAY_SUBSYSTEM"),
            display_refresh_hz: env_f64("DECKCRAFT_DISPLAY_REFRESH_HZ"),
            device_pixel_ratio: env_f64("DECKCRAFT_DEVICE_PIXEL_RATIO"),
        }
    }
}

fn env_or(name: &str, default: &str) -> String {
    env_opt(name).unwrap_or_else(|| default.to_owned())
}

fn env_opt(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn env_f64(name: &str) -> Option<f64> {
    env_opt(name)?.trim().parse::<f64>().ok().filter(|value| value.is_finite())
}

fn build_profile() -> &'static str {
    if cfg!(debug_assertions) { "debug" } else { "release" }
}

/// The `resource` every record carries: who produced it and what made this run distinct.
fn resource(identity: &RunIdentity) -> Resource {
    let mut attributes = vec![
        KeyValue::new(sc::SERVICE_VERSION, env!("CARGO_PKG_VERSION")),
        KeyValue::new(sc::DEPLOYMENT_ENVIRONMENT_NAME, identity.environment.clone()),
        KeyValue::new(sc::A_BUILD_PROFILE, build_profile()),
        KeyValue::new(sc::A_TEST_RUN_ID, identity.run_id.clone()),
        KeyValue::new(sc::A_TEST_VARIANT, identity.variant.clone()),
        KeyValue::new(sc::A_TEST_ZOOM, identity.zoom),
        KeyValue::new(sc::A_HW_GPU_ADAPTER, identity.gpu_adapter.clone()),
        KeyValue::new(sc::A_RENDERER_BACKEND, identity.renderer_backend.clone()),
    ];
    if let Some(vsync) = &identity.display_vsync {
        attributes.push(KeyValue::new(sc::A_DISPLAY_VSYNC, vsync.clone()));
    }
    if let Some(subsystem) = &identity.display_subsystem {
        attributes.push(KeyValue::new(sc::A_DISPLAY_SUBSYSTEM, subsystem.clone()));
    }
    if let Some(hz) = identity.display_refresh_hz {
        attributes.push(KeyValue::new(sc::A_DISPLAY_REFRESH_HZ, hz));
    }
    if let Some(ratio) = identity.device_pixel_ratio {
        attributes.push(KeyValue::new(sc::A_DEVICE_PIXEL_RATIO, ratio));
    }
    Resource::builder().with_service_name(sc::SERVICE_NAME_VALUE).with_attributes(attributes).build()
}

/// Install `meter` as the global provider and bind the shared instrument handles to it.
///
/// The two calls cannot be swapped and cannot be split: `global::meter()` binds a `Meter` to the
/// provider that is current at that instant, and `Instruments::new` builds every handle from that
/// `Meter`. Bind first and the handles are no-ops for the life of the process — silently, with
/// every gate green.
fn install_meter_provider(meter: &SdkMeterProvider) {
    global::set_meter_provider(meter.clone());
    deckcraft_telemetry::init_instruments();
}

/// The providers, owned by the app so they outlive every frame and can be flushed on exit.
pub struct Providers {
    tracer: SdkTracerProvider,
    logger: SdkLoggerProvider,
    meter: SdkMeterProvider,
}

impl Providers {
    /// Flush and stop every provider, in the order the official examples use.
    ///
    /// Shutting the tracer down first drains the spans the frame spans are still holding; the
    /// meter goes last because metric handles are cheap to keep recording into a shutting-down
    /// provider, while a span exporter that is already gone would reject them.
    pub fn shutdown(self) {
        if let Err(error) = self.tracer.shutdown() {
            log::warn!("telemetry: tracer shutdown: {error}");
        }
        if let Err(error) = self.logger.shutdown() {
            log::warn!("telemetry: logger shutdown: {error}");
        }
        if let Err(error) = self.meter.shutdown() {
            log::warn!("telemetry: meter shutdown: {error}");
        }
    }
}

/// True when an OTLP collector was actually pointed at.
///
/// The OTLP spec defaults to `http://localhost:4318`, and the SDK applies that default. This app
/// does not *assume* a collector: with nothing listening, every export retries, fails, and logs
/// two records — per second, into the user's own log file. A developer who set `DECKCRAFT_OTEL`
/// only to get the file sink would drown in that. So the HTTP exporter exists when one of the
/// standard endpoint variables says where to send, and the file sink covers everything else.
fn otlp_endpoint_configured() -> bool {
    ["OTEL_EXPORTER_OTLP_ENDPOINT", "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT"]
        .iter()
        .any(|name| env_opt(name).is_some())
}

/// Build the SDK, install the providers globally and bridge `log` into OTel.
///
/// `None` means telemetry stays off (no sink configured, or an exporter could not be built); the
/// app keeps running. `app_logger` is the logger already installed by [`crate::logging::install`],
/// which forwards every accepted record into the bridge — the `log` crate has room for one global
/// logger and that slot is taken.
pub fn init(app_logger: Option<&'static AppLogger>) -> Option<Providers> {
    let resource = resource(&RunIdentity::from_env());
    let file_dir = std::env::var_os("DECKCRAFT_OTEL_DIR").map(PathBuf::from);
    let otlp = otlp_endpoint_configured();
    if !otlp && file_dir.is_none() {
        log::warn!(
            "telemetry: DECKCRAFT_OTEL is set but neither an OTEL_EXPORTER_OTLP_*_ENDPOINT nor DECKCRAFT_OTEL_DIR is; nothing will be exported"
        );
        return None;
    }

    let mut meter_builder = SdkMeterProvider::builder().with_resource(resource.clone());
    if otlp {
        match MetricExporter::builder().with_http().with_protocol(Protocol::HttpBinary).build() {
            Ok(exporter) => meter_builder = meter_builder.with_periodic_exporter(exporter),
            Err(error) => {
                log::error!("telemetry: the OTLP/HTTP metric exporter could not be built: {error}");
                return None;
            }
        }
    }
    if let Some(dir) = &file_dir {
        match FileMetricExporter::create(dir.join(METRICS_FILE)) {
            Ok(exporter) => meter_builder = meter_builder.with_periodic_exporter(exporter),
            Err(error) => log::warn!("telemetry: {METRICS_FILE} will not be written: {error}"),
        }
    }
    let meter = meter_builder.build();
    install_meter_provider(&meter);

    let mut tracer_builder = SdkTracerProvider::builder().with_resource(resource.clone());
    if otlp {
        match SpanExporter::builder().with_http().with_protocol(Protocol::HttpBinary).build() {
            Ok(exporter) => tracer_builder = tracer_builder.with_batch_exporter(exporter),
            Err(error) => {
                log::error!("telemetry: the OTLP/HTTP span exporter could not be built: {error}");
                return None;
            }
        }
    }
    if let Some(dir) = &file_dir {
        match FileSpanExporter::create(dir.join(TRACES_FILE)) {
            Ok(exporter) => tracer_builder = tracer_builder.with_batch_exporter(exporter),
            Err(error) => log::warn!("telemetry: {TRACES_FILE} will not be written: {error}"),
        }
    }
    let tracer = tracer_builder.build();
    // Without this every `global::tracer()` in `crates/telemetry` stays the no-op one: metrics
    // would export and `traces.jsonl` would stay empty.
    global::set_tracer_provider(tracer.clone());

    let mut logger_builder = SdkLoggerProvider::builder().with_resource(resource);
    if otlp {
        match LogExporter::builder().with_http().with_protocol(Protocol::HttpBinary).build() {
            Ok(exporter) => logger_builder = logger_builder.with_batch_exporter(exporter),
            Err(error) => {
                log::error!("telemetry: the OTLP/HTTP log exporter could not be built: {error}");
                return None;
            }
        }
    }
    if let Some(dir) = &file_dir {
        match FileLogExporter::create(dir.join(LOGS_FILE)) {
            Ok(exporter) => logger_builder = logger_builder.with_batch_exporter(exporter),
            Err(error) => log::warn!("telemetry: {LOGS_FILE} will not be written: {error}"),
        }
    }
    let logger = logger_builder.build();

    if let Some(app_logger) = app_logger {
        app_logger.attach_tee(Box::new(BridgeTee::new(&logger)));
    }
    log::info!("telemetry: ready (OTLP/HTTP: {otlp}, file sink: {file_dir:?})");

    Some(Providers { tracer, logger, meter })
}

thread_local! {
    /// Set while a record is inside the bridge. The bridge and the SDK log through `log` as well,
    /// so without this a record produced while handling a record recurses.
    static FORWARDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Clears [`FORWARDING`] even if the bridge unwinds, so one bad record cannot silence the thread.
struct ForwardingGuard;

impl Drop for ForwardingGuard {
    fn drop(&mut self) {
        FORWARDING.with(|flag| flag.set(false));
    }
}

/// The `log` → OTel bridge, taught not to feed on itself.
///
/// Every `otel_error!`/`otel_debug!` inside the SDK re-enters the `log` crate with a target that
/// starts with `opentelemetry`. Forwarding those would make an export failure produce a log
/// record, whose export fails too: a feedback loop, and a deadlock once the provider is shutting
/// down and the worker thread is the one logging. They stay in the app's own stderr/file sinks.
struct BridgeTee<P, L>(OpenTelemetryLogBridge<P, L>)
where
    P: LoggerProvider<Logger = L> + Send + Sync,
    L: Logger + Send + Sync;

impl<P, L> BridgeTee<P, L>
where
    P: LoggerProvider<Logger = L> + Send + Sync,
    L: Logger + Send + Sync,
{
    fn new(provider: &P) -> BridgeTee<P, L> {
        BridgeTee(OpenTelemetryLogBridge::new(provider))
    }
}

impl<P, L> Tee for BridgeTee<P, L>
where
    P: LoggerProvider<Logger = L> + Send + Sync,
    L: Logger + Send + Sync,
{
    fn log(&self, record: &log::Record<'_>) {
        if record.target().starts_with("opentelemetry") {
            return;
        }
        // `try_with` instead of `with`: a control-client thread can log while its TLS is being
        // destroyed, where `with` panics. If the flag is unreachable, skip forwarding rather than
        // panic on a thread that is already exiting.
        let nested = FORWARDING.try_with(|flag| flag.replace(true)).unwrap_or(true);
        if nested {
            return;
        }
        let _guard = ForwardingGuard;
        log::Log::log(&self.0, record);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use opentelemetry::Value;
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
    use opentelemetry_sdk::trace::SdkTracerProvider;

    use super::*;

    fn identity() -> RunIdentity {
        RunIdentity {
            environment: "test".to_owned(),
            run_id: "run-7".to_owned(),
            variant: "candidate".to_owned(),
            zoom: 1.5,
            gpu_adapter: "llvmpipe".to_owned(),
            renderer_backend: "vulkan".to_owned(),
            display_vsync: Some("off".to_owned()),
            display_subsystem: Some("wayland".to_owned()),
            display_refresh_hz: Some(144.0),
            device_pixel_ratio: None,
        }
    }

    fn attributes(resource: &Resource) -> HashMap<String, Value> {
        resource.iter().map(|(key, value)| (key.to_string(), value.clone())).collect()
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("deckcraft-otel-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Proves the global tracer wiring end to end. The span helpers in `crates/telemetry` ask for
    /// `global::tracer()` on every call, so a span created after `global::set_tracer_provider` is
    /// exported — while without that call they stay no-ops and `traces.jsonl` stays empty even
    /// though metrics keep working. No compiler and no other test catches that.
    #[test]
    fn spans_from_the_shared_helpers_reach_the_exported_file() {
        let dir = temp_dir("global-tracer");
        let path = dir.join("traces.jsonl");
        let exporter = crate::telemetry_file_exporter::FileSpanExporter::create(&path).expect("create the span exporter");
        let provider = SdkTracerProvider::builder().with_batch_exporter(exporter).build();
        // Process-global and set once; no other test in this binary installs a tracer provider.
        global::set_tracer_provider(provider.clone());

        deckcraft_telemetry::spans::phase_span(sc::P_BUILD, || ());
        provider.force_flush().expect("flush the batch processor");

        let text = std::fs::read_to_string(&path).expect("read traces.jsonl");
        let record: serde_json::Value = serde_json::from_str(text.lines().next().expect("a batch line")).expect("OTLP/JSON");
        let span = &record["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert!(span["name"].as_str().is_some_and(|name| name.ends_with(sc::P_BUILD)), "the phase span was exported: {span}");
        assert!(
            span["attributes"]
                .as_array()
                .is_some_and(|attributes| attributes.iter().any(|kv| kv["key"] == sc::A_PHASE && kv["value"]["stringValue"] == sc::P_BUILD)),
            "the phase attribute survived the conversion: {span}"
        );
    }

    #[test]
    fn the_resource_carries_the_run_identity_verbatim() {
        let found = attributes(&resource(&identity()));

        assert_eq!(found.get(sc::SERVICE_NAME), Some(&Value::from(sc::SERVICE_NAME_VALUE)), "service.name comes from with_service_name");
        assert_eq!(found.get(sc::SERVICE_VERSION), Some(&Value::from(env!("CARGO_PKG_VERSION"))));
        assert_eq!(found.get(sc::DEPLOYMENT_ENVIRONMENT_NAME), Some(&Value::from("test")));
        assert!(
            matches!(found.get(sc::A_BUILD_PROFILE), Some(Value::String(profile)) if profile.as_str() == "debug" || profile.as_str() == "release")
        );
        assert_eq!(found.get(sc::A_TEST_RUN_ID), Some(&Value::from("run-7")));
        assert_eq!(found.get(sc::A_TEST_VARIANT), Some(&Value::from("candidate")));
        assert_eq!(found.get(sc::A_TEST_ZOOM), Some(&Value::F64(1.5)));
        assert_eq!(found.get(sc::A_HW_GPU_ADAPTER), Some(&Value::from("llvmpipe")));
        assert_eq!(found.get(sc::A_RENDERER_BACKEND), Some(&Value::from("vulkan")));
        assert_eq!(found.get(sc::A_DISPLAY_VSYNC), Some(&Value::from("off")));
        assert_eq!(found.get(sc::A_DISPLAY_SUBSYSTEM), Some(&Value::from("wayland")));
        assert_eq!(found.get(sc::A_DISPLAY_REFRESH_HZ), Some(&Value::F64(144.0)));
        assert!(!found.contains_key(sc::A_DEVICE_PIXEL_RATIO), "an unknown display value is absent, never guessed: {found:?}");
    }

    /// The ordering rule the SDK imposes: a `Meter` (and every instrument built from it) binds to
    /// the provider that is global *when it is created*. Installing the provider first and binding
    /// the shared handles second is what makes `instruments().frame_duration` land on a real
    /// exporter instead of the no-op one — a failure that no compiler and no gate catches.
    #[test]
    fn instruments_bind_to_the_provider_installed_before_them() {
        // This test owns the process-global meter provider: no other test in this binary installs
        // one, and a second `set_meter_provider` is ignored by the API.
        let memory = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder().with_periodic_exporter(memory.clone()).build();
        install_meter_provider(&provider);

        deckcraft_telemetry::instruments().frame_duration.record(16.7, &[]);
        provider.force_flush().expect("flush the in-memory reader");

        let batches = memory.get_finished_metrics().expect("collect");
        let names: Vec<&str> =
            batches.iter().flat_map(|batch| batch.scope_metrics()).flat_map(|scope| scope.metrics()).map(|metric| metric.name()).collect();
        assert!(names.contains(&sc::M_FRAME_DURATION), "the handle must be bound to the installed provider, not the no-op one: {names:?}");
    }
}
