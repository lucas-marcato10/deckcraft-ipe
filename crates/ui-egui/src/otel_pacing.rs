//! Frame pacing verdicts: the **tearing proxies**.
//!
//! These are proxies, not proof. Optical tearing needs `wp_presentation` timing on Wayland or an
//! external capture of the panel; from inside the app all we have is the interval between two frame
//! starts, which can only say that a present was late, duplicated or split — never that the user
//! saw a torn image. Treat a non-zero counter as "look here", not as a verdict.
//!
//! The verdict is a pure function of the display interval, the frame delta and the swapchain's
//! present mode, so it is unit-tested without a window and the same numbers feed the histogram, the
//! counter and the span event.

use std::sync::OnceLock;

use deckcraft_telemetry::sc;
use opentelemetry::KeyValue;

/// What one frame interval says about presentation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Verdict {
    /// `dt - interval`: positive means the frame took longer than one refresh interval.
    pub error_ms: f64,
    /// The frame missed its vsync deadline (more than one interval plus half an interval of
    /// jitter tolerance).
    pub late: bool,
    /// One present carried stale pixels: a single update straddled a refresh boundary.
    pub split: bool,
    /// The app presented faster than the display could consume, so the same content was shown
    /// twice. Only reachable without vsync.
    pub duplicate: bool,
}

/// Jitter tolerance, in refresh intervals. Half an interval absorbs timer and scheduler noise
/// without hiding a real missed vsync.
const TOLERANCE: f64 = 0.5;

/// Upper end of a *split* present, in intervals.
///
/// Between 1.5 and 2.5 intervals exactly one refresh was presented with stale pixels: the update
/// straddled that boundary. Past 2.5 the frame simply stalled for several refreshes, which is a
/// stall (and `late`), not a split.
const SPLIT_END: f64 = 2.5;

/// Classify one frame interval against the display interval.
///
/// A non-finite or non-positive `interval_ms` and a non-finite `dt_ms` yield an empty verdict:
/// inputs are hostile and a fabricated verdict would poison the report.
pub fn pacing(interval_ms: f64, dt_ms: f64, present_mode: &str) -> Verdict {
    if !interval_ms.is_finite() || interval_ms <= 0.0 || !dt_ms.is_finite() {
        return Verdict { error_ms: 0.0, late: false, split: false, duplicate: false };
    }
    let intervals = dt_ms / interval_ms;
    let late = intervals > 1.0 + TOLERANCE;
    Verdict {
        error_ms: dt_ms - interval_ms,
        late,
        split: late && intervals < SPLIT_END,
        // `immediate` and friends do not wait for the display, so a frame may be presented twice
        // inside one refresh; a vsync-locked mode cannot do that, whatever the delta says.
        duplicate: !vsync_locked(present_mode) && intervals < 1.0 - TOLERANCE,
    }
}

/// Whether `present_mode` waits for the display (`fifo`, `mailbox`).
fn vsync_locked(present_mode: &str) -> bool {
    !present_mode.eq_ignore_ascii_case("immediate")
}

/// The display interval the verdict compares against: `DECKCRAFT_DISPLAY_REFRESH_HZ` when it is a
/// usable number, 60 Hz otherwise.
///
/// eframe exposes no API for the real refresh rate of the window's monitor, so the value comes from
/// the run's environment, like the rest of the resource identity. A wrong value makes every verdict
/// wrong — which is why a run whose environment differs is refused as an A/B baseline elsewhere.
pub fn interval_ms() -> f64 {
    static INTERVAL: OnceLock<f64> = OnceLock::new();
    *INTERVAL.get_or_init(|| {
        std::env::var("DECKCRAFT_DISPLAY_REFRESH_HZ")
            .ok()
            .and_then(|hz| hz.trim().parse::<f64>().ok())
            .filter(|hz| hz.is_finite() && *hz > 0.0)
            .map(|hz| 1000.0 / hz)
            .unwrap_or(1000.0 / 60.0)
    })
}

/// The swapchain's present mode, from `DECKCRAFT_PRESENT_MODE` (fifo when unset).
///
/// Only the known modes are accepted: an unknown string would let a typo decide whether duplicates
/// are possible, so it falls back to the wgpu default instead.
pub fn present_mode() -> &'static str {
    static MODE: OnceLock<&'static str> = OnceLock::new();
    MODE.get_or_init(|| match std::env::var("DECKCRAFT_PRESENT_MODE").unwrap_or_default().trim().to_ascii_lowercase().as_str() {
        "immediate" => "immediate",
        "mailbox" => "mailbox",
        "relaxed" => "relaxed",
        _ => "fifo",
    })
}

/// Emit one frame's verdict: the pacing error, one counter per symptom, and one span event per
/// symptom on the enclosing `deckcraft.frame` span.
///
/// The counters are proxies (see the module docs). They are separate from `jank_class`, which
/// measures how long the frame took; this measures how it landed relative to the display.
pub fn record(verdict: &Verdict) {
    let instruments = deckcraft_telemetry::instruments();
    instruments.pacing_error.record(verdict.error_ms, &[]);
    let proxies = [(verdict.late, sc::V_TEARING_LATE), (verdict.split, sc::V_TEARING_SPLIT), (verdict.duplicate, sc::V_TEARING_DUPLICATE)];
    for (present, proxy) in proxies {
        if present {
            deckcraft_telemetry::spans::event(sc::E_TEARING_SUSPECT, vec![KeyValue::new(sc::A_TEARING_PROXY, proxy)]);
        }
    }
    if verdict.late {
        instruments.frame_late.add(1, &[]);
    }
    if verdict.split {
        instruments.frame_split.add(1, &[]);
    }
    if verdict.duplicate {
        instruments.frame_duplicate.add(1, &[]);
    }
}

/// The single jank class of a frame, by severity.
///
/// Exclusive on purpose: the report adds the three counters up to "janky frames", so a frame that
/// missed two vsyncs *and* ran for a second must be counted once, in the worst class it earned.
pub fn jank_class(frame_ms: f64) -> Option<&'static str> {
    if !frame_ms.is_finite() {
        return None;
    }
    if frame_ms > sc::LONG_TASK_MS {
        Some(sc::V_JANK_LONG_TASK)
    } else if frame_ms > 2.0 * sc::FRAME_BUDGET_MS {
        Some(sc::V_JANK_DROPPED_FRAME)
    } else if frame_ms > sc::FRAME_BUDGET_MS {
        Some(sc::V_JANK_MISSED_VSYNC)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The jank classes are exclusive and ordered by severity: a 60 ms frame is a long task, not
    /// three counters adding to one. The report sums them to the number of janky frames, so a
    /// frame counted twice would make the total exceed the frame count.
    #[test]
    fn a_frame_is_classified_into_exactly_one_jank_class() {
        assert_eq!(jank_class(3.0), None);
        assert_eq!(jank_class(16.7), None, "exactly the budget is not janky");
        assert_eq!(jank_class(20.0), Some(deckcraft_telemetry::sc::V_JANK_MISSED_VSYNC));
        assert_eq!(jank_class(33.3), Some(deckcraft_telemetry::sc::V_JANK_MISSED_VSYNC), "the boundary belongs to the lower class");
        assert_eq!(jank_class(40.0), Some(deckcraft_telemetry::sc::V_JANK_DROPPED_FRAME));
        assert_eq!(jank_class(50.0), Some(deckcraft_telemetry::sc::V_JANK_DROPPED_FRAME));
        assert_eq!(jank_class(60.0), Some(deckcraft_telemetry::sc::V_JANK_LONG_TASK));
    }

    /// A frame that lands on the interval is not late, not split and not duplicated. This is the
    /// control: without it a verdict that always returns `true` would pass the other two tests.
    #[test]
    fn a_frame_on_the_display_interval_is_clean() {
        let verdict = pacing(16.667, 16.7, "fifo");
        assert!(!verdict.late, "16.7 ms on a 16.667 ms interval is not late: {verdict:?}");
        assert!(!verdict.split, "nothing was split: {verdict:?}");
        assert!(!verdict.duplicate, "a vsync-locked present cannot duplicate: {verdict:?}");
        assert!((verdict.error_ms - 0.033).abs() < 1e-3, "error must be dt - interval: {verdict:?}");
    }

    /// Two intervals of work for one interval of display: the frame missed a vsync and the present
    /// that should have carried this update showed the previous one.
    #[test]
    fn a_frame_that_takes_two_intervals_is_late_and_split() {
        let verdict = pacing(16.667, 34.0, "fifo");
        assert!(verdict.late, "34 ms is more than one interval and a half: {verdict:?}");
        assert!(verdict.split, "one refresh was presented with stale pixels: {verdict:?}");
        assert!(!verdict.duplicate, "fifo waits for the display: {verdict:?}");
        assert!((verdict.error_ms - 17.333).abs() < 1e-3, "error must be dt - interval: {verdict:?}");
    }

    /// Without vsync the app can present twice inside one refresh; the compositor shows the same
    /// content twice. This is the class that only `immediate` can produce.
    #[test]
    fn a_frame_faster_than_the_display_duplicates_without_vsync() {
        let verdict = pacing(16.667, 8.0, "immediate");
        assert!(verdict.duplicate, "8 ms with no vsync presents twice per refresh: {verdict:?}");
        assert!(!verdict.late, "it was early, not late: {verdict:?}");
        assert!(!verdict.split, "nothing straddled a boundary: {verdict:?}");
    }

    /// Hostile inputs are not allowed to produce a number: a zero interval would divide by zero and
    /// a NaN would land in the first bucket as if it were a real sample.
    #[test]
    fn a_hostile_interval_yields_no_verdict() {
        for (interval, dt) in [(0.0, 16.0), (-16.0, 16.0), (f64::NAN, 16.0), (16.667, f64::INFINITY)] {
            assert_eq!(pacing(interval, dt, "fifo"), Verdict { error_ms: 0.0, late: false, split: false, duplicate: false });
        }
    }
}
