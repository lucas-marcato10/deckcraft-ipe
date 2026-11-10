//! Startup measures: process → first painted frame, and process → first accepted input.
//!
//! Both are once-per-process, and both are measured from an instant taken at the top of `main()` —
//! not from the app's construction. The window server, the driver and argument parsing all happen
//! before the app exists, and they are part of what the user waited for.
//!
//! The host that owns `main()` reports the instant with [`SlideApp::mark_process_start`]. A host
//! that never does (the test harness) records nothing: a start time invented here would be a
//! fabricated sample in a histogram the report trusts.

use crate::SlideApp;

impl SlideApp {
    /// Take the process start instant, and restart the two startup samples.
    ///
    /// Clearing the flags matters because a host may have painted (or a harness laid out) before it
    /// reads its clock: marking the start is the host saying "measure from here", so the first
    /// frame and input after it are the ones that count.
    pub fn mark_process_start(&mut self, at: web_time::Instant) {
        self.process_start = Some(at);
        self.startup_frame_sampled = false;
        self.startup_input_sampled = false;
    }

    /// Record `startup.time_to_first_frame` on the first painted frame. Later frames are free.
    pub(crate) fn startup_first_frame(&mut self) {
        if self.startup_frame_sampled {
            return;
        }
        self.startup_frame_sampled = true;
        if let Some(ms) = self.startup_elapsed_ms() {
            deckcraft_telemetry::instruments().time_to_first_frame.record(ms, &[]);
        }
    }

    /// Record `startup.time_to_interactive` on the first accepted input. A frame that is merely
    /// painted is not an app the user can use yet, which is why this is a second sample.
    pub(crate) fn startup_first_input(&mut self) {
        if self.startup_input_sampled {
            return;
        }
        self.startup_input_sampled = true;
        if let Some(ms) = self.startup_elapsed_ms() {
            deckcraft_telemetry::instruments().time_to_interactive.record(ms, &[]);
        }
    }

    /// Milliseconds since the process started, when the host reported it.
    fn startup_elapsed_ms(&self) -> Option<f64> {
        let start = self.process_start?;
        Some((start.elapsed().as_secs_f64() * 1000.0).max(0.0))
    }
}
