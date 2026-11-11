//! Guards the span helpers against the OTel trace API.
//!
//! This crate cannot depend on the SDK (L0, wasm-safe), so the tests install a **recording**
//! `Tracer`: it implements `opentelemetry::trace::Span` and keeps names, attributes, events and
//! end-calls in memory. That makes the assertions real — "the frame span carries `frame.id=7` and
//! the raster phase is its child" is observed, not assumed — without smuggling an exporter into
//! the layer that must stay API-only.

use std::borrow::Cow;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use deckcraft_telemetry::sc;
use deckcraft_telemetry::spans::{self, Inp};
use opentelemetry::trace::{Span, SpanBuilder, SpanContext, Status, TraceContextExt, Tracer, TracerProvider};
use opentelemetry::{Context, InstrumentationScope, KeyValue, Value};

/// One span as the recorder saw it.
#[derive(Clone, Debug, Default)]
struct Recorded {
    name: String,
    /// Whether the span was started while another span was active — the nesting check.
    parent_active: bool,
    attributes: Vec<KeyValue>,
    events: Vec<String>,
    ended: bool,
}

type Shared = Arc<Mutex<Recorded>>;

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic in one assertion must not turn every later assertion into a poison error.
    value.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn string_attr(record: &Recorded, key: &str) -> String {
    let found = record.attributes.iter().find(|kv| kv.key.as_str() == key);
    match found.map(|kv| &kv.value) {
        Some(Value::String(value)) => value.as_str().to_owned(),
        other => panic!("attribute {key} is not a string: {other:?}"),
    }
}

fn f64_attr(record: &Recorded, key: &str) -> f64 {
    let found = record.attributes.iter().find(|kv| kv.key.as_str() == key);
    match found.map(|kv| &kv.value) {
        Some(Value::F64(value)) => *value,
        other => panic!("attribute {key} is not an f64: {other:?}"),
    }
}

fn i64_attr(record: &Recorded, key: &str) -> i64 {
    let found = record.attributes.iter().find(|kv| kv.key.as_str() == key);
    match found.map(|kv| &kv.value) {
        Some(Value::I64(value)) => *value,
        other => panic!("attribute {key} is not an i64: {other:?}"),
    }
}

#[derive(Debug, Clone, Default)]
struct RecordingTracer {
    spans: Arc<Mutex<Vec<Shared>>>,
}

impl RecordingTracer {
    /// A snapshot, never a live guard: holding a record's lock while searching would deadlock the
    /// moment a test asks for a second span.
    fn snapshot(&self, name: &str) -> Option<Recorded> {
        let spans: Vec<Shared> = lock(&self.spans).clone();
        spans.iter().find(|span| lock(span).name == name).map(|span| lock(span).clone())
    }

    fn expect_span(&self, name: &str) -> Recorded {
        self.snapshot(name).unwrap_or_else(|| panic!("span {name} was never started"))
    }

    fn has_span(&self, name: &str) -> bool {
        self.snapshot(name).is_some()
    }
}

impl Tracer for RecordingTracer {
    type Span = RecordingSpan;

    fn build_with_context(&self, builder: SpanBuilder, parent_cx: &Context) -> RecordingSpan {
        let record = Arc::new(Mutex::new(Recorded {
            name: builder.name.into_owned(),
            parent_active: parent_cx.has_active_span(),
            attributes: builder.attributes.unwrap_or_default(),
            ..Recorded::default()
        }));
        lock(&self.spans).push(Arc::clone(&record));
        RecordingSpan { record, context: SpanContext::empty_context() }
    }
}

impl TracerProvider for RecordingTracer {
    type Tracer = RecordingTracer;

    fn tracer_with_scope(&self, _scope: InstrumentationScope) -> Self::Tracer {
        self.clone()
    }
}

#[derive(Debug)]
struct RecordingSpan {
    record: Shared,
    context: SpanContext,
}

impl Span for RecordingSpan {
    fn add_event_with_timestamp<T>(&mut self, name: T, _timestamp: SystemTime, attributes: Vec<KeyValue>)
    where
        T: Into<Cow<'static, str>>,
    {
        let mut record = lock(&self.record);
        record.events.push(name.into().into_owned());
        record.attributes.extend(attributes);
    }

    fn span_context(&self) -> &SpanContext {
        &self.context
    }

    fn is_recording(&self) -> bool {
        !lock(&self.record).ended
    }

    fn set_attribute(&mut self, attribute: KeyValue) {
        lock(&self.record).attributes.push(attribute);
    }

    fn set_status(&mut self, _status: Status) {}

    fn update_name<T>(&mut self, new_name: T)
    where
        T: Into<Cow<'static, str>>,
    {
        lock(&self.record).name = new_name.into().into_owned();
    }

    fn add_link(&mut self, _span_context: SpanContext, _attributes: Vec<KeyValue>) {}

    fn end_with_timestamp(&mut self, _timestamp: SystemTime) {
        lock(&self.record).ended = true;
    }
}

/// The cross-frame helpers talk to the *global* tracer, so a test that exercises them installs
/// its recorder globally. Global state is process-wide: serialise those tests, or two of them
/// would fight over the provider and over the pending-interaction map.
fn with_global_recorder<T>(body: impl FnOnce(&RecordingTracer) -> T) -> T {
    static SERIAL: Mutex<()> = Mutex::new(());
    let _serial = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let tracer = RecordingTracer::default();
    opentelemetry::global::set_tracer_provider(tracer.clone());
    body(&tracer)
}

#[test]
fn the_frame_span_is_the_named_root_carrying_its_attributes() {
    let tracer = RecordingTracer::default();
    let attrs = vec![KeyValue::new(sc::A_FRAME_ID, 7_i64), KeyValue::new(sc::A_FRAME_KIND, "interactive")];

    let returned = spans::frame_span_with(&tracer, attrs, |cx| {
        // The body runs with the frame span active, so anything it emits lands on the frame.
        assert!(cx.has_active_span(), "the frame span must be active inside the body");
        41 + 1
    });

    assert_eq!(returned, 42, "frame_span must return the body's value");
    let frame = tracer.expect_span(sc::S_FRAME);
    assert_eq!(i64_attr(&frame, sc::A_FRAME_ID), 7);
    assert_eq!(string_attr(&frame, sc::A_FRAME_KIND), "interactive");
    assert!(!frame.parent_active, "the frame span is the root of the trace");
}

#[test]
fn phase_spans_are_named_after_the_phase_and_nest_under_the_frame() {
    let tracer = RecordingTracer::default();

    spans::frame_span_with(&tracer, Vec::new(), |_cx| {
        // A phase span created inside the frame must be its child; that is what makes the
        // report's "which phase is the bottleneck" question answerable per frame.
        spans::phase_span_with(&tracer, sc::P_RASTER, || 1);
        spans::phase_span_with(&tracer, sc::P_UPLOAD, || 2);
    });

    let name = format!("{}{}", sc::S_PHASE_PREFIX, sc::P_RASTER);
    let raster = tracer.expect_span(&name);
    assert_eq!(string_attr(&raster, sc::A_PHASE), sc::P_RASTER);
    assert!(raster.parent_active, "a phase span must be a child of the frame span");

    let upload_name = format!("{}{}", sc::S_PHASE_PREFIX, sc::P_UPLOAD);
    assert!(tracer.has_span(&upload_name), "every phase gets its own span name");
}

#[test]
fn events_land_on_the_active_span() {
    let tracer = RecordingTracer::default();

    spans::phase_span_with(&tracer, sc::P_RASTER, || {
        spans::event(sc::E_CACHE_MISS, vec![KeyValue::new(sc::A_CACHE_REASON, sc::V_REASON_SIZE)]);
    });

    let name = format!("{}{}", sc::S_PHASE_PREFIX, sc::P_RASTER);
    let phase = tracer.expect_span(&name);
    assert_eq!(phase.events, vec![sc::E_CACHE_MISS.to_owned()]);
    assert_eq!(string_attr(&phase, sc::A_CACHE_REASON), sc::V_REASON_SIZE);
}

#[test]
fn a_single_frame_interaction_records_every_inp_phase() {
    let tracer = RecordingTracer::default();
    let inp = Inp { input_delay_ms: 0.8, processing_ms: 19.4, presentation_ms: 5.8 };

    let returned = spans::interaction_span_with(&tracer, "click", &inp, |_cx| "painted");

    assert_eq!(returned, "painted");
    let span = tracer.expect_span(sc::S_INTERACTION);
    assert_eq!(string_attr(&span, sc::A_INTERACTION_TYPE), "click");
    assert_eq!(f64_attr(&span, sc::A_INP_INPUT_DELAY_MS), 0.8);
    assert_eq!(f64_attr(&span, sc::A_INP_PROCESSING_MS), 19.4);
    assert_eq!(f64_attr(&span, sc::A_INP_PRESENTATION_MS), 5.8);
    let total = f64_attr(&span, sc::A_INP_TOTAL_MS);
    assert!((total - 26.0).abs() < 1e-9, "INP total must be the sum of its phases, got {total}");
}

#[test]
fn a_cross_frame_interaction_is_ended_at_paint_with_all_three_phases() {
    with_global_recorder(|tracer| {
        // Input arrives at frame N, the paint happens at frame N+k: the span is started at input
        // and ended at paint, so its duration *is* the INP. Timestamps come from the caller's
        // monotonic clock; the span's own start/end times are the SDK's business.
        spans::interaction_begin("drag-1", "drag", 1_000.0);
        assert_eq!(spans::pending_interactions(), 1);

        spans::interaction_dispatched("drag-1", 1_004.0);
        spans::interaction_processed("drag-1", 1_012.0);
        let inp = spans::interaction_painted("drag-1", 1_030.0).expect("painting a known interaction yields its INP");

        assert_eq!(spans::pending_interactions(), 0, "the pending entry must not leak");
        assert!((inp.input_delay_ms - 4.0).abs() < 1e-9);
        assert!((inp.processing_ms - 8.0).abs() < 1e-9);
        assert!((inp.presentation_ms - 18.0).abs() < 1e-9);

        let span = tracer.expect_span(sc::S_INTERACTION);
        assert!(span.ended, "the span must be ended at paint, not when the gesture began");
        assert_eq!(string_attr(&span, sc::A_INTERACTION_TYPE), "drag");
        assert!((f64_attr(&span, sc::A_INP_TOTAL_MS) - 30.0).abs() < 1e-9);
    });
}

#[test]
fn interaction_phase_events_for_an_unknown_key_are_ignored() {
    with_global_recorder(|_tracer| {
        // A paint without a matching begin (a resized window, a dropped event) must never panic
        // and must never invent a span: the interaction histogram simply gets no sample.
        assert!(spans::interaction_painted("ghost", 5.0).is_none());
        spans::interaction_dispatched("ghost", 6.0);
        spans::interaction_processed("ghost", 7.0);
        assert_eq!(spans::pending_interactions(), 0);
    });
}

#[test]
fn pending_interactions_stay_bounded_when_paints_never_arrive() {
    with_global_recorder(|_tracer| {
        // A gesture interrupted by a closed window would otherwise grow the map forever. The cap
        // ends the oldest span, so the loss is visible instead of silent.
        for n in 0..(spans::MAX_PENDING_INTERACTIONS * 2) {
            spans::interaction_begin(&format!("orphan-{n}"), "drag", n as f64);
        }
        assert_eq!(spans::pending_interactions(), spans::MAX_PENDING_INTERACTIONS);

        // Drain, so this test leaves no state behind for whichever global test runs next.
        for n in 0..(spans::MAX_PENDING_INTERACTIONS * 2) {
            let _ = spans::interaction_painted(&format!("orphan-{n}"), 0.0);
        }
        assert_eq!(spans::pending_interactions(), 0);
    });
}

/// The map is shared by every UI thread: a gesture that begins on one thread is painted on
/// another. Eight threads driving 200 complete interactions each must neither lose a paint (a
/// begin without its painted is a leaked span) nor deadlock on the map or on the span ends that
/// happen outside it.
///
/// Deliberately inside `with_global_recorder`: the recorder serialises every test that touches the
/// process-wide pending map, and this one touches it hardest. Without that, the concurrent churn
/// would interleave with the invariant assertions of the three tests above.
#[test]
fn concurrent_interactions_never_leak_or_deadlock() {
    with_global_recorder(|_tracer| {
        let mut handles = Vec::new();
        for t in 0..8 {
            handles.push(std::thread::spawn(move || {
                for i in 0..200 {
                    let key = format!("t{t}-{i}");
                    spans::interaction_begin(&key, "drag", i as f64);
                    spans::interaction_dispatched(&key, i as f64 + 0.1);
                    let inp = spans::interaction_painted(&key, i as f64 + 1.0);
                    assert!(inp.is_some(), "cada begin deve ter seu painted");
                }
            }));
        }
        for handle in handles {
            handle.join().expect("worker panicou");
        }
        assert_eq!(spans::pending_interactions(), 0);
        assert!(spans::pending_interactions() <= spans::MAX_PENDING_INTERACTIONS);
    });
}
