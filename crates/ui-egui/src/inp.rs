//! INP bookkeeping for interactions that are painted by a later frame.
//!
//! An interaction starts at the event and is painted by a frame that has not been built yet, so its
//! three phases are measured in two places: the control channel or the raw-input hook starts it,
//! and the frame that closes records the end. [`SlideApp::interaction_begin`] parks the key here;
//! the frame methods below dispatch, process and paint it in order, and only
//! [`SlideApp::interaction_painted`] records the samples — one `interaction.duration` and three
//! `inp.phase` points, from the same numbers, so the breakdown always adds up to the total.
//!
//! # What each phase means here
//!
//! * `input_delay` — the event timestamp to the pass that handled it.
//! * `processing` — the handler deciding to paint. In an immediate-mode UI this is the same tick
//!   as the dispatch, so it is honestly near zero rather than a guessed number.
//! * `presentation` — from that decision to the frame closing. The backend offers no presentation
//!   callback, so this ends at [`SlideApp::finish_frame`] — *before* the compositor shows the
//!   pixels. The total is therefore a lower bound on the optical latency; it does not prove what
//!   the user saw, and it cannot see a frame the compositor dropped.
//!
//! # What is not covered
//!
//! A request that is rejected *before* it is dispatched is refused without opening an interaction
//! (the control arms validate first). A dispatch that fails *after* the interaction opened — a
//! command error inside `Session::pointer` or `Session::key` — was already counted, and the next
//! painted frame closes it: the sample says "the app took this long to answer", which for a failed
//! request is the time the user also waited.

use deckcraft_telemetry::sc;
use opentelemetry::KeyValue;

use crate::SlideApp;

/// One interaction waiting for the frame that paints it.
pub(crate) struct PendingInteraction {
    /// The span key: unique per interaction, and the handle the frame closes it with.
    key: String,
    /// `interaction.type` — also the attribute of the samples this interaction records.
    kind: &'static str,
    /// The event reached the handler.
    dispatched: bool,
    /// The handler asked for the frame that will paint it.
    processed: bool,
}

impl SlideApp {
    /// Start an interaction at `at_ms` (a monotonic millisecond timestamp from the same clock as
    /// the frame) and count it. Returns the key that later closes it.
    ///
    /// Every interaction is counted here, so the count cannot drift between the control channel
    /// and real input; `coalesced_events` is recorded by the caller that batched several events
    /// into one paint.
    pub(crate) fn interaction_begin(&mut self, kind: &'static str, at_ms: f64) -> String {
        // The first accepted input is what makes the app interactive, whatever route it came by.
        self.startup_first_input();
        self.interaction_seq = self.interaction_seq.wrapping_add(1);
        let key = format!("{kind}:{}", self.interaction_seq);
        deckcraft_telemetry::spans::interaction_begin(&key, kind, at_ms);
        deckcraft_telemetry::instruments().interaction_count.add(1, &[KeyValue::new(sc::A_INTERACTION_TYPE, kind)]);
        self.interactions.push(PendingInteraction { key: key.clone(), kind, dispatched: false, processed: false });
        key
    }

    /// The event reached the handler (the start of the pass that consumes it).
    pub(crate) fn interaction_dispatched(&mut self, key: &str, at_ms: f64) {
        if let Some(pending) = self.interactions.iter_mut().find(|pending| pending.key == key) {
            pending.dispatched = true;
        }
        deckcraft_telemetry::spans::interaction_dispatched(key, at_ms);
    }

    /// The handler asked for the frame that will paint the result.
    pub(crate) fn interaction_processed(&mut self, key: &str, at_ms: f64) {
        if let Some(pending) = self.interactions.iter_mut().find(|pending| pending.key == key) {
            pending.processed = true;
        }
        deckcraft_telemetry::spans::interaction_processed(key, at_ms);
    }

    /// Start of a painted frame: an interaction queued by raw input has now reached the pass that
    /// consumes it, so its handling starts — and in an immediate-mode UI its frame is requested in
    /// the same tick. Control-channel interactions were dispatched and processed when the request
    /// ran, so they are left alone: overwriting their timestamps here would hide the wait between
    /// the request and the frame in `input_delay`.
    pub(crate) fn interaction_frame_started(&mut self, now_ms: f64) {
        for pending in &mut self.interactions {
            if pending.dispatched {
                continue;
            }
            pending.dispatched = true;
            pending.processed = true;
            deckcraft_telemetry::spans::interaction_dispatched(&pending.key, now_ms);
            deckcraft_telemetry::spans::interaction_processed(&pending.key, now_ms);
        }
    }

    /// Close every pending interaction at frame close and record one INP sample each.
    ///
    /// This is the only place the interaction histograms are written: the span it ends carries the
    /// same phases as attributes, so a trace and the report can never disagree.
    pub(crate) fn interaction_painted(&mut self, now_ms: f64) {
        if self.interactions.is_empty() {
            return;
        }
        let instruments = deckcraft_telemetry::instruments();
        let pending: Vec<PendingInteraction> = std::mem::take(&mut self.interactions);
        for entry in pending {
            let Some(inp) = deckcraft_telemetry::spans::interaction_painted(&entry.key, now_ms) else {
                // The pending map evicted it (too many interactions without a paint): the span was
                // ended there, and inventing a sample here would fabricate a latency.
                continue;
            };
            let kind = KeyValue::new(sc::A_INTERACTION_TYPE, entry.kind);
            instruments.interaction_duration.record(inp.total_ms(), std::slice::from_ref(&kind));
            for (phase, value) in
                [(sc::P_INPUT_DELAY, inp.input_delay_ms), (sc::P_PROCESSING, inp.processing_ms), (sc::P_PRESENTATION, inp.presentation_ms)]
            {
                instruments.interaction_phase.record(value, &[KeyValue::new(sc::A_INP_PHASE, phase)]);
            }
        }
    }
}
