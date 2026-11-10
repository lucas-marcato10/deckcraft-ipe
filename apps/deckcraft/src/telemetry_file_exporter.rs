//! OTLP/JSON Lines file exporters: one JSON object per line, one file per signal.
//!
//! The [OTLP File Exporter] spec is the reason this module exists: a collector is not always
//! running (CI, a developer's laptop, a deterministic A/B run) and the SDK has no exporter that
//! writes to disk. `opentelemetry-otlp-file` is **not published** — crates.io answers 404 for it
//! and the `opentelemetry-rust` repository has no `v0.33.1` tag (`git ls-remote --tags` stops at
//! `v0.31.0`), so a git-pinned dependency is not an option either.
//!
//! These are still SDK exporters, not a parallel pipeline: each one implements the SDK's own
//! exporter trait (`PushMetricExporter`, `SpanExporter`, `LogExporter`) and receives the batches
//! the SDK's `PeriodicReader`/`BatchSpanProcessor` already aggregated, on the SDK's own threads.
//! The payload is the OTLP/JSON encoding of the proto messages, produced by
//! `opentelemetry-proto`'s own SDK→proto transforms — the same bytes the HTTP/JSON transport would
//! put on the wire — so the file is readable by any OTLP receiver (`otlpjsonfilereceiver`).
//!
//! [OTLP File Exporter]: https://opentelemetry.io/docs/specs/otel/protocol/file-exporter/

use std::fs::File;
use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::transform::common::tonic::ResourceAttributesWithSchema;
use opentelemetry_proto::transform::logs::tonic::group_logs_by_resource_and_scope;
use opentelemetry_proto::transform::trace::tonic::group_spans_by_resource_and_scope;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::logs::{LogBatch, LogExporter};
use opentelemetry_sdk::metrics::Temporality;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::trace::{SpanData, SpanExporter};

/// One OTLP/JSON Lines file: an append-only handle that writes one JSON object per line.
///
/// Serialising happens *before* the lock and the write is a single `write_all` of the finished
/// line, so a slow or contended writer can never leave half a record behind for the reader.
#[derive(Debug)]
struct JsonLinesFile {
    path: PathBuf,
    file: Mutex<File>,
}

impl JsonLinesFile {
    fn create(path: PathBuf) -> std::io::Result<JsonLinesFile> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Created eagerly: an empty file is the honest record of "telemetry was on and nothing was
        // recorded", which is different from "telemetry was off" (no file at all).
        let file = File::options().create(true).append(true).open(&path)?;
        Ok(JsonLinesFile { path, file: Mutex::new(file) })
    }

    fn append(&self, line: &[u8]) -> OTelSdkResult {
        let mut record = Vec::with_capacity(line.len() + 1);
        record.extend_from_slice(line);
        record.push(b'\n');
        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        file.write_all(&record).map_err(|error| OTelSdkError::InternalFailure(format!("{}: {error}", self.path.display())))
    }
}

/// Writes metric batches as OTLP/JSON Lines (`metrics.jsonl`).
#[derive(Debug)]
pub struct FileMetricExporter {
    file: JsonLinesFile,
}

impl FileMetricExporter {
    pub fn create(path: impl Into<PathBuf>) -> std::io::Result<FileMetricExporter> {
        Ok(FileMetricExporter { file: JsonLinesFile::create(path.into())? })
    }
}

impl PushMetricExporter for FileMetricExporter {
    fn export(&self, metrics: &ResourceMetrics) -> impl Future<Output = OTelSdkResult> + Send {
        // Built before the future: the batch borrowing is done synchronously, exactly as the HTTP
        // exporter does, so only owned data crosses the (unused) await point.
        let request = ExportMetricsServiceRequest::from(metrics);
        async move {
            let line = serde_json::to_vec(&request).map_err(|error| OTelSdkError::InternalFailure(format!("OTLP/JSON: {error}")))?;
            self.file.append(&line)
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        // Every record is written through to the OS by `append`; there is no buffer to drain.
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }

    fn temporality(&self) -> Temporality {
        Temporality::Cumulative
    }
}

/// Writes span batches as OTLP/JSON Lines (`traces.jsonl`).
#[derive(Debug)]
pub struct FileSpanExporter {
    file: JsonLinesFile,
    /// Set by the SDK through [`SpanExporter::set_resource`] before the first export: unlike a
    /// metric batch, a bare `SpanData` carries no resource, so the exporter is the one that
    /// remembers it — exactly what the OTLP exporters do.
    resource: Option<ResourceAttributesWithSchema>,
}

impl FileSpanExporter {
    pub fn create(path: impl Into<PathBuf>) -> std::io::Result<FileSpanExporter> {
        Ok(FileSpanExporter { file: JsonLinesFile::create(path.into())?, resource: None })
    }
}

impl SpanExporter for FileSpanExporter {
    fn export(&self, batch: Vec<SpanData>) -> impl Future<Output = OTelSdkResult> + Send {
        // Grouped before the future so the batch is consumed here and only owned proto data
        // crosses the await point.
        let request = match &self.resource {
            Some(resource) => ExportTraceServiceRequest { resource_spans: group_spans_by_resource_and_scope(batch, resource) },
            None => ExportTraceServiceRequest::default(),
        };
        async move {
            let line = serde_json::to_vec(&request).map_err(|error| OTelSdkError::InternalFailure(format!("OTLP/JSON: {error}")))?;
            self.file.append(&line)
        }
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.resource = Some(ResourceAttributesWithSchema::from(resource));
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }
}

/// Writes log batches as OTLP/JSON Lines (`logs.jsonl`).
#[derive(Debug)]
pub struct FileLogExporter {
    file: JsonLinesFile,
    /// Filled by [`LogExporter::set_resource`]; a `LogBatch` carries no resource of its own.
    resource: Option<ResourceAttributesWithSchema>,
}

impl FileLogExporter {
    pub fn create(path: impl Into<PathBuf>) -> std::io::Result<FileLogExporter> {
        Ok(FileLogExporter { file: JsonLinesFile::create(path.into())?, resource: None })
    }
}

impl LogExporter for FileLogExporter {
    fn export(&self, batch: LogBatch<'_>) -> impl Future<Output = OTelSdkResult> + Send {
        let request = match &self.resource {
            Some(resource) => ExportLogsServiceRequest { resource_logs: group_logs_by_resource_and_scope(&batch, resource) },
            None => ExportLogsServiceRequest::default(),
        };
        async move {
            let line = serde_json::to_vec(&request).map_err(|error| OTelSdkError::InternalFailure(format!("OTLP/JSON: {error}")))?;
            self.file.append(&line)
        }
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.resource = Some(ResourceAttributesWithSchema::from(resource));
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use opentelemetry::KeyValue;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry::trace::Tracer as _;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_appender_log::OpenTelemetryLogBridge;
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::logs::SdkLoggerProvider;
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
    use opentelemetry_sdk::trace::SdkTracerProvider;

    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("deckcraft-otel-file-{tag}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Every line of `path`, parsed. A line that is not a JSON object fails here, which is the
    /// property the collector's receiver depends on.
    fn read_json_lines(path: &Path) -> Vec<serde_json::Value> {
        let text = std::fs::read_to_string(path).expect("read jsonl");
        text.lines().map(|line| serde_json::from_str(line).expect("each line is JSON")).collect()
    }

    /// Poll a future that is known to complete without yielding (every exporter here is a file
    /// write). No runtime is involved, which is the point: these exporters must work without one.
    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(value) => value,
            std::task::Poll::Pending => panic!("a file write must not yield"),
        }
    }

    fn test_resource() -> Resource {
        Resource::builder().with_service_name("deckcraft-test").with_attribute(KeyValue::new("test.variant", "baseline")).build()
    }

    #[test]
    fn logs_export_appends_resource_logs_through_the_log_bridge() {
        let dir = temp_dir("logs");
        let path = dir.join("logs.jsonl");
        let exporter = FileLogExporter::create(&path).expect("create the log exporter");

        // The exact production path: `log` records go through the official appender into the SDK
        // logger provider, and the file exporter receives the batch.
        let provider = SdkLoggerProvider::builder().with_batch_exporter(exporter).with_resource(test_resource()).build();
        let bridge = OpenTelemetryLogBridge::new(&provider);
        let record =
            log::Record::builder().level(log::Level::Warn).target("egui_wgpu").args(format_args!("There are 4 available wgpu adapters")).build();
        log::Log::log(&bridge, &record);
        provider.force_flush().expect("flush the log processor");

        let records = read_json_lines(&path);
        assert_eq!(records.len(), 1, "one line for the flushed batch: {records:?}");
        let root = &records[0];
        assert!(root.get("resourceLogs").is_some(), "OTLP/JSON top-level key: {root}");
        let log_record = &root["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
        assert_eq!(log_record["body"]["stringValue"], "There are 4 available wgpu adapters");
        assert_eq!(log_record["severityText"], "WARN");
        assert_eq!(log_record["severityNumber"], 13, "OTel severity number for WARN: {log_record}");
    }

    #[test]
    fn spans_export_appends_resource_spans_with_the_span_name() {
        let dir = temp_dir("traces");
        let path = dir.join("traces.jsonl");
        let exporter = FileSpanExporter::create(&path).expect("create the span exporter");

        let provider = SdkTracerProvider::builder().with_batch_exporter(exporter).with_resource(test_resource()).build();
        let tracer = provider.tracer("deckcraft-file-exporter-test");
        tracer.in_span("deckcraft.frame", |_cx| {});
        provider.force_flush().expect("flush the batch processor");

        let records = read_json_lines(&path);
        assert_eq!(records.len(), 1, "one line for the flushed batch: {records:?}");
        let root = &records[0];
        assert!(root.get("resourceSpans").is_some(), "OTLP/JSON top-level key: {root}");
        let span = &root["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(span["name"], "deckcraft.frame");
        assert!(span["traceId"].as_str().is_some_and(|id| !id.is_empty()), "trace id is present: {span}");
    }

    #[test]
    fn metrics_export_appends_one_otlp_json_line_per_batch() {
        let dir = temp_dir("metrics");
        let path = dir.join("metrics.jsonl");
        let exporter = FileMetricExporter::create(&path).expect("create the metrics exporter");

        // A real provider records into the in-memory exporter; the batches it produces are what
        // the PeriodicReader would have handed the file exporter.
        let memory = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder().with_periodic_exporter(memory.clone()).with_resource(test_resource()).build();
        provider.meter("deckcraft-file-exporter-test").u64_counter("deckcraft.test.records").build().add(1, &[KeyValue::new("k", "v")]);
        provider.force_flush().expect("flush the in-memory reader");

        let batches = memory.get_finished_metrics().expect("collect");
        assert_eq!(batches.len(), 1, "one collection, one batch");
        block_on(exporter.export(&batches[0])).expect("export the batch");
        block_on(exporter.export(&batches[0])).expect("export the batch again");

        let records = read_json_lines(&path);
        assert_eq!(records.len(), 2, "one line per exported batch: {records:?}");
        let root = &records[0];
        assert!(root.get("resourceMetrics").is_some(), "OTLP/JSON top-level key: {root}");
        let metrics = &root["resourceMetrics"][0]["scopeMetrics"][0]["metrics"];
        assert_eq!(metrics[0]["name"], "deckcraft.test.records");
        let attributes = &root["resourceMetrics"][0]["resource"]["attributes"];
        assert!(
            attributes.as_array().is_some_and(|a| a.iter().any(|kv| kv["key"] == "service.name" && kv["value"]["stringValue"] == "deckcraft-test")),
            "resource attributes carry service.name: {attributes}"
        );
    }
}
