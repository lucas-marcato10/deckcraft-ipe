//! OpenTelemetry semantic conventions and instrument handles for DeckCraft frame diagnostics.
//!
//! This crate is **L0** and wasm-safe: it depends on the OTel *API* only
//! (`default-features = false`), never on the SDK, an exporter or a runtime. The SDK is wired at
//! the application boundary (`apps/deckcraft`) so libraries can emit into a no-op provider at no
//! cost when telemetry is off.
//!
//! [`sc`] holds every name, unit, bucket bound and attribute key; nothing below the app may spell
//! one by hand.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

pub mod semconv;

/// Short alias for [`semconv`] — every call site writes `sc::M_FRAME_DURATION`.
pub use semconv as sc;

/// Crate version, mirrored into `scope.version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
