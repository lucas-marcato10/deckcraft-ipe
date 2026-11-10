//! OpenTelemetry semantic conventions and instrument handles for DeckCraft frame diagnostics.
//!
//! This crate is **L0** and wasm-safe: it depends on the OTel *API* only
//! (`default-features = false`), never on the SDK, an exporter or a runtime. The SDK is wired at
//! the application boundary (`apps/deckcraft`) so libraries can emit into a no-op provider at no
//! cost when telemetry is off.
//!
//! [`sc`] holds every name, unit, bucket bound and attribute key; nothing below the app may spell
//! one by hand. [`instruments`] turns those constants into typed handles.
//!
//! # Ordering, the one rule that bites
//!
//! `opentelemetry` binds a [`Meter`](opentelemetry::metrics::Meter) to the provider configured at
//! the moment it is obtained: a meter grabbed before `set_meter_provider` stays no-op forever.
//! That is why the app sets the provider and then calls [`init_instruments()`] before the first
//! frame — [`instruments()`] alone would bind to whatever provider exists at first use. Span
//! helpers avoid the trap by asking for the global tracer on every call, so spans emitted after a
//! late `set_tracer_provider` are still exported; metrics are sampled away, spans are not.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

use std::sync::OnceLock;

pub mod instruments;
pub mod semconv;
pub mod spans;

/// Short alias for [`semconv`] — every call site writes `sc::M_FRAME_DURATION`.
pub use semconv as sc;

/// Crate version, mirrored into `scope.version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The one `Instruments` set per process.
///
/// Held behind a `OnceLock` rather than rebuilt per call site because a handle rebuilt against a
/// no-op meter is silently worthless: one binding point makes the ordering rule auditable.
static INSTRUMENTS: OnceLock<instruments::Instruments> = OnceLock::new();

/// The global handle set, built from the global `Meter` on first use.
///
/// Call sites use `instruments().frame_duration.record(ms, &[..])`. In the app, [`init_instruments()`]
/// has already run, so this returns the handles bound to the real SDK provider.
pub fn instruments() -> &'static instruments::Instruments {
    INSTRUMENTS.get_or_init(|| instruments::Instruments::new(&opentelemetry::global::meter(sc::SCOPE_NAME)))
}

/// Bind the global handle set to the current global `Meter` provider.
///
/// Must be called by the application **after** `global::set_meter_provider(..)` and before the
/// first frame. Idempotent: a second call (or a call after [`instruments()`] already initialised
/// the set) keeps the original handles, so a test or a second window cannot swap instruments out
/// from under call sites that already hold a `&'static` reference.
pub fn init_instruments() {
    // A failed `set` means the set was already initialised; that is the idempotent path, not an
    // error, so the `Result` is deliberately ignored.
    let _ = INSTRUMENTS.set(instruments::Instruments::new(&opentelemetry::global::meter(sc::SCOPE_NAME)));
}
