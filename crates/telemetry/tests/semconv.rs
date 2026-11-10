//! Guards the semantic conventions: the bounds must split at the 60 Hz budget and every
//! instrument name must be namespaced and declared once.

use std::collections::HashSet;

use deckcraft_telemetry::sc;

#[test]
fn frame_and_inp_bounds_split_at_one_and_two_vsyncs_and_increase() {
    for bounds in [sc::FRAME_BOUNDS_MS, sc::INP_BOUNDS_MS] {
        assert!(bounds.contains(&16.7), "bounds must split at one 60 Hz frame: {bounds:?}");
        assert!(bounds.contains(&33.3), "bounds must split at two 60 Hz frames: {bounds:?}");
        assert!(bounds.windows(2).all(|pair| pair[0] < pair[1]), "bounds must strictly increase: {bounds:?}");
    }
    // The budget must be a bucket edge, so "how many frames missed it" is an exact histogram count
    // instead of an interpolation between two buckets.
    assert!(sc::FRAME_BOUNDS_MS.contains(&sc::FRAME_BUDGET_MS), "the frame budget must be a bucket edge");
    // Compile-time invariant: a long task always outlasts a frame.
    const { assert!(sc::LONG_TASK_MS >= sc::FRAME_BUDGET_MS) };
}

#[test]
fn metric_names_are_namespaced_and_unique() {
    let mut seen = HashSet::new();
    for name in sc::METRIC_NAMES {
        assert!(name.starts_with("deckcraft."), "{name} must live under the deckcraft. namespace");
        assert!(seen.insert(*name), "{name} is declared more than once");
    }
    assert!(seen.len() >= 29, "the P0 instrument table declares 29 metrics, found {}", seen.len());
}
