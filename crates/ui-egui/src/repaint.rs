//! `request_repaint*` calls, attributed to the source that asked for the next frame.
//!
//! Why a frame ran is not visible in a timestamp: a 16 ms interval looks identical whether a GIF
//! asked for it, the control channel queued a request, or the slide show is mid-animation. The
//! counter turns that into a rate per source, which is what tells a report "the editor repainted
//! 60 times a second with nothing to do" from "the editor repainted because there was work".
//!
//! Only the five sources `deckcraft-telemetry` declares are attributed: the GIF tick, the
//! animation preview, the continuous paint loop of a running show, synthetic input, and the
//! control channel (counted where a request is consumed, [`control_consumed`]). Other repaint
//! requests (caret blink, autosave deadline, a thumbnail budget still filling) have no declared
//! source and stay plain `egui` calls — a sixth value invented at a call site would produce counts
//! no query knows how to read.

use deckcraft_telemetry::{instruments, sc};
use opentelemetry::KeyValue;

/// Count one repaint request for `source`.
fn count(source: &'static str) {
    // A slice literal: no `format!`, no `Vec` allocation on a path that runs every frame.
    instruments().repaint_requested.add(1, &[KeyValue::new(sc::A_REPAINT_SOURCE, source)]);
}

/// A GIF frame is due in `wait`: the animation clock asked for that deadline.
pub(crate) fn gif_after(ctx: &egui::Context, wait: std::time::Duration) {
    count(sc::V_REPAINT_GIF);
    ctx.request_repaint_after(wait);
}

/// The in-canvas animation preview advanced: the next frame shows it.
pub(crate) fn anim_preview(ctx: &egui::Context) {
    count(sc::V_REPAINT_ANIM_PREVIEW);
    ctx.request_repaint();
}

/// A control request was consumed by this frame.
///
/// The wake itself happens in the desktop app's control server
/// (`apps/deckcraft/src/control_server.rs`): that is the single place a request wakes the UI.
/// Counting here, where the request is actually handled, keeps the number in the crate that owns
/// the instrument and attributes it to the frame that paid for it; a request that timed out before
/// any frame took it is never counted, which is the honest answer.
pub(crate) fn control_consumed() {
    count(sc::V_REPAINT_CONTROL);
}

/// A running slide show is animating (a transition, a morph or a triggered animation): the show
/// paints continuously until the step settles.
pub(crate) fn continuous(ctx: &egui::Context) {
    count(sc::V_REPAINT_CONTINUOUS);
    ctx.request_repaint();
}

/// Synthetic input is queued and has not been consumed by a UI pass yet.
pub(crate) fn synthetic(ctx: &egui::Context) {
    count(sc::V_REPAINT_SYNTHETIC);
    ctx.request_repaint();
}
