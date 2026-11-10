//! Span helpers on the OTel `Tracer`, following the pattern in the official examples:
//! `tracer.in_span(name, |cx| { cx.span().set_attribute(..); .. })`.
//!
//! # What lives here and what does not
//!
//! Only span *shape*: names from [`crate::sc`], the attributes each span must carry, and where a
//! span starts and ends. W3C trace/span ids, start and end timestamps, sampling, batching and
//! export are the SDK's job — this crate never synthesises an id or a clock reading. The `inp.*`
//! numbers are the exception that proves the rule: they are *measurements* taken by the caller
//! (the UI's monotonic clock) and attached as attributes, not span timestamps.
//!
//! # Interactions that span frames
//!
//! An interaction starts at input and is painted at frame N+k, so its span cannot be scoped to one
//! function call. [`interaction_begin`] starts the span and parks it in a bounded map;
//! [`interaction_painted`] attaches the three INP phases, ends it and returns them, so the caller
//! can record the authoritative [`crate::instruments::Instruments::interaction_duration`] sample
//! from the same numbers. If a paint never arrives, the entry is ended by the oldest-first eviction
//! at [`MAX_PENDING_INTERACTIONS`] — a lost interaction shows up as an ended span, not as a leak.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};

use opentelemetry::global::BoxedSpan;
use opentelemetry::trace::{Span, Tracer, get_active_span};
use opentelemetry::{Context, KeyValue};

use crate::sc;

/// Upper bound on interaction spans waiting for a paint.
///
/// A window closed mid-drag, a gesture whose paint never came: without a bound the map would grow
/// for the lifetime of the process. 64 is far more than one gesture chain can produce, and the
/// eviction ends the oldest span so the loss is visible in the trace instead of silent.
pub const MAX_PENDING_INTERACTIONS: usize = 64;

/// The three INP phases, in milliseconds, measured by the caller.
///
/// Phases are measured with a monotonic clock (`now_ms`) and clamped at zero: a caller that hands
/// the same event to two frames could otherwise produce a negative latency, which no histogram
/// bucket can represent meaningfully.
#[derive(Clone, Copy, Debug, Default)]
pub struct Inp {
    /// Event arrival to handler start.
    pub input_delay_ms: f64,
    /// Handler start to the frame that consumes the input.
    pub processing_ms: f64,
    /// That frame's start to the pixels being presented.
    pub presentation_ms: f64,
}

impl Inp {
    /// The INP number itself: the sum of the three phases.
    ///
    /// Summing the (clamped) phases rather than measuring event-to-paint separately keeps the
    /// report's phase breakdown adding up to its total, which is the whole point of the breakdown.
    pub fn total_ms(&self) -> f64 {
        self.input_delay_ms + self.processing_ms + self.presentation_ms
    }

    /// The four `inp.*` span attributes, in phase order.
    fn attributes(&self) -> Vec<KeyValue> {
        vec![
            KeyValue::new(sc::A_INP_INPUT_DELAY_MS, self.input_delay_ms),
            KeyValue::new(sc::A_INP_PROCESSING_MS, self.processing_ms),
            KeyValue::new(sc::A_INP_PRESENTATION_MS, self.presentation_ms),
            KeyValue::new(sc::A_INP_TOTAL_MS, self.total_ms()),
        ]
    }
}

/// The global tracer, resolved per call.
///
/// Deliberately not cached: the global API binds a tracer to the provider configured at that
/// moment, so a cached tracer built before the app installs its SDK provider would stay no-op
/// forever. One global read per span is cheap next to the frame that emits it.
fn tracer() -> opentelemetry::global::BoxedTracer {
    opentelemetry::global::tracer(sc::SCOPE_NAME)
}

/// Run `body` inside the root `deckcraft.frame` span, carrying `attrs` from the start.
///
/// Attributes are passed to the span builder rather than set inside the body so they exist at
/// creation time (a sampler and the exporter both see them), and so a frame that is painted has
/// its identifying attributes even if the body panics.
pub fn frame_span<R>(attrs: Vec<KeyValue>, body: impl FnOnce(Context) -> R) -> R {
    frame_span_with(&tracer(), attrs, body)
}

/// [`frame_span`] against an explicit tracer — the seam the tests use.
pub fn frame_span_with<T, R>(tracer: &T, attrs: Vec<KeyValue>, body: impl FnOnce(Context) -> R) -> R
where
    T: Tracer,
    T::Span: Send + Sync + 'static,
{
    let builder = tracer.span_builder(sc::S_FRAME).with_attributes(attrs);
    tracer.in_span_with_builder(builder, body)
}

/// Run `body` inside a `deckcraft.phase.<phase>` child span of the active frame.
///
/// One span per phase, named after the `phase` attribute — the report groups by that attribute, so
/// the two can never drift apart.
pub fn phase_span<R>(phase: &'static str, body: impl FnOnce() -> R) -> R {
    phase_span_with(&tracer(), phase, body)
}

/// [`phase_span`] against an explicit tracer — the seam the tests use.
pub fn phase_span_with<T, R>(tracer: &T, phase: &'static str, body: impl FnOnce() -> R) -> R
where
    T: Tracer,
    T::Span: Send + Sync + 'static,
{
    let name = format!("{}{}", sc::S_PHASE_PREFIX, phase);
    let builder = tracer.span_builder(name).with_attributes(vec![KeyValue::new(sc::A_PHASE, phase)]);
    tracer.in_span_with_builder(builder, |_cx| body())
}

/// Record an event on the currently active span (frame, phase or interaction).
///
/// Deep call sites — a cache miss inside the texture pipeline, a tearing suspect at present time —
/// have no `Context` at hand; the active-span helper is the documented way to reach the enclosing
/// span without threading one through.
pub fn event(name: &'static str, attrs: Vec<KeyValue>) {
    get_active_span(move |span| span.add_event(name, attrs));
}

/// Run `body` inside a `deckcraft.interaction` span for an interaction resolved within one frame.
///
/// `inp` carries timings the caller already measured; for a gesture that crosses frames use
/// [`interaction_begin`] / [`interaction_painted`] instead.
pub fn interaction_span<R>(kind: &'static str, inp: &Inp, body: impl FnOnce(Context) -> R) -> R {
    interaction_span_with(&tracer(), kind, inp, body)
}

/// [`interaction_span`] against an explicit tracer — the seam the tests use.
pub fn interaction_span_with<T, R>(tracer: &T, kind: &'static str, inp: &Inp, body: impl FnOnce(Context) -> R) -> R
where
    T: Tracer,
    T::Span: Send + Sync + 'static,
{
    let mut attributes = vec![KeyValue::new(sc::A_INTERACTION_TYPE, kind)];
    attributes.extend(inp.attributes());
    let builder = tracer.span_builder(sc::S_INTERACTION).with_attributes(attributes);
    tracer.in_span_with_builder(builder, body)
}

/// An interaction span that is still open, waiting for the frame that paints it.
struct Pending {
    span: BoxedSpan,
    started_ms: f64,
    dispatched_ms: Option<f64>,
    processed_ms: Option<f64>,
}

impl Pending {
    /// Turn the timestamps collected so far into INP phases.
    ///
    /// A missing `dispatched`/`processed` collapses the phases it cannot separate instead of
    /// dropping the sample: an interaction that reached the paint still produced a total.
    fn inp(&self, painted_ms: f64) -> Inp {
        let dispatched_ms = self.dispatched_ms.unwrap_or(painted_ms);
        let processed_ms = self.processed_ms.unwrap_or(dispatched_ms);
        Inp {
            input_delay_ms: elapsed_ms(self.started_ms, dispatched_ms),
            processing_ms: elapsed_ms(dispatched_ms, processed_ms),
            presentation_ms: elapsed_ms(processed_ms, painted_ms),
        }
    }
}

/// A monotonic delta that cannot go negative, whatever order the caller reports events in.
fn elapsed_ms(from_ms: f64, to_ms: f64) -> f64 {
    (to_ms - from_ms).max(0.0)
}

/// The pending-interaction map.
///
/// Poisoning is handled by taking the inner value: a panic while holding the lock must not turn
/// every later interaction into a panic in a UI thread.
fn pending() -> MutexGuard<'static, HashMap<String, Pending>> {
    static PENDING: OnceLock<Mutex<HashMap<String, Pending>>> = OnceLock::new();
    let map = PENDING.get_or_init(|| Mutex::new(HashMap::new()));
    map.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Start the span of an interaction that may not be painted until a later frame.
///
/// `key` identifies the gesture (the caller's event id); `now_ms` is the caller's monotonic
/// timestamp for the event. Re-beginning the same key, or exceeding [`MAX_PENDING_INTERACTIONS`],
/// ends the oldest open span so nothing is silently leaked.
pub fn interaction_begin(key: &str, kind: &'static str, now_ms: f64) {
    let mut span = tracer().start(sc::S_INTERACTION);
    span.set_attribute(KeyValue::new(sc::A_INTERACTION_TYPE, kind));

    let entry = Pending { span, started_ms: now_ms, dispatched_ms: None, processed_ms: None };
    let mut ended: Vec<BoxedSpan> = Vec::new();
    {
        let mut pending = pending();
        if pending.len() >= MAX_PENDING_INTERACTIONS {
            let oldest = oldest_key(&pending);
            if let Some(oldest) = oldest
                && let Some(stale) = pending.remove(&oldest)
            {
                ended.push(stale.span);
            }
        }
        if let Some(replaced) = pending.insert(key.to_owned(), entry) {
            // The same gesture began twice: the interrupted span is ended, not overwritten.
            ended.push(replaced.span);
        }
    }
    // Ending a span reaches the SDK's processor. Never do that while holding the map: a slow
    // exporter would stall every other interaction on the frame thread.
    for mut stale in ended {
        stale.end();
    }
}

/// The key of the interaction that has been waiting longest.
fn oldest_key(map: &HashMap<String, Pending>) -> Option<String> {
    map.iter().min_by(|left, right| left.1.started_ms.total_cmp(&right.1.started_ms)).map(|(key, _)| key.clone())
}

/// Record that the input reached the handler of an open interaction.
///
/// Unknown keys are ignored: a resized window or a coalesced event must never panic the UI.
pub fn interaction_dispatched(key: &str, now_ms: f64) {
    let mut pending = pending();
    if let Some(entry) = pending.get_mut(key) {
        entry.dispatched_ms = Some(now_ms);
    }
}

/// Record that the handler of an open interaction asked for the frame that will paint it.
pub fn interaction_processed(key: &str, now_ms: f64) {
    let mut pending = pending();
    if let Some(entry) = pending.get_mut(key) {
        entry.processed_ms = Some(now_ms);
    }
}

/// End the span of `key` at paint time and return its INP phases.
///
/// Returns `None` when no interaction with that key is open — a paint without an input is not an
/// error, it simply has no sample. The caller records the authoritative
/// [`crate::instruments::Instruments::interaction_duration`] from the returned [`Inp`].
pub fn interaction_painted(key: &str, now_ms: f64) -> Option<Inp> {
    let mut entry = {
        let mut pending = pending();
        pending.remove(key)?
    };
    let inp = entry.inp(now_ms);
    for attribute in inp.attributes() {
        entry.span.set_attribute(attribute);
    }
    entry.span.end();
    Some(inp)
}

/// How many interaction spans are waiting for a paint — the input-backlog gauge, and a leak check.
pub fn pending_interactions() -> usize {
    pending().len()
}
