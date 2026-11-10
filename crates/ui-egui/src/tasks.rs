//! Named main-thread tasks.
//!
//! A frame that took 300 ms tells the report nothing on its own; `task.name` says which operation
//! held the thread. The histogram is the attribution, and the `deckcraft.long_task` span event (over
//! [`deckcraft_telemetry::sc::LONG_TASK_MS`]) is what makes a stall visible in a trace next to the
//! frame it delayed.
//!
//! Only main-thread work belongs here. An operation on a worker thread (a background file read)
//! would inflate the tail with time no frame waited for.

use deckcraft_telemetry::sc;
use opentelemetry::KeyValue;

/// Record one named task that started at `started_ms` on the shared [`crate::now_ms`] clock.
pub(crate) fn record(name: &'static str, started_ms: f64) {
    record_elapsed(name, crate::now_ms() - started_ms);
}

/// Record one named task whose duration is already measured.
pub(crate) fn record_elapsed(name: &'static str, elapsed_ms: f64) {
    let elapsed = if elapsed_ms.is_finite() { elapsed_ms.max(0.0) } else { return };
    let instruments = deckcraft_telemetry::instruments();
    instruments.task_duration.record(elapsed, &[KeyValue::new(sc::A_TASK_NAME, name)]);
    if elapsed > sc::LONG_TASK_MS {
        // The event lands on whatever span encloses the call — in practice the frame that stalled.
        deckcraft_telemetry::spans::event(sc::E_LONG_TASK, vec![KeyValue::new(sc::A_TASK_NAME, name)]);
        log::warn!("long task {name}: {elapsed:.1} ms");
    }
}
