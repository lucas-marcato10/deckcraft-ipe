# DeckCraft smart guides

> **Last reviewed:** 2026-11-06 · **Last updated:** 2026-11-06 · **Change:** initial (snap priority, multi-win + full-span guides, equal-gap spacing, temporal hysteresis, sort+binary window) · **Target:** Microsoft PowerPoint / Figma smart guides

How snapping and smart guides work, why the algorithm is shaped the way it is, and what is
deliberately left out. The code is `crates/engine/src/tools/geometry.rs` (`snap_edges`,
`snap_resize`), with drag wiring in `crates/engine/src/tools/drag.rs`, gesture state in
`crates/engine/src/tools.rs`, and rendering in `crates/ui-egui/src/canvas/overlays.rs`.
Tests: `crates/engine/src/tests.rs` (`move_prefers_neighbor_over_slide_center_on_tie`,
`smart_guides_show_both_edges_when_both_align`, `spacing_snaps_to_equal_gap_and_reports_it`,
`move_snaps_the_bounds_not_the_pressed_spot`, the `snap_hysteresis_*` family).

## The model: axis-aligned snap is two 1-D problems

An unrotated box snaps by its left/centre/right and top/centre/bottom lines. X never constrains Y,
so snapping is solved as **two independent 1-D matchings** (`snap_edges` runs once per axis), not as
a 2-D spatial query. This is why the pipeline is a sorted candidate list plus a search, and not an
R-tree or a spatial hash: those answer "what is near this point in 2-D?", while snap answers
"which object has this exact coordinate on this axis, within tolerance?".

Per axis the pipeline is:

1. **Collect** candidates once per frame: the slide's own lines (`0`, `W/2`, `W`) at priority 2,
   manual drawing guides at priority 1, and each visible non-selected shape's three lines at
   priority 0. Degenerate boxes (non-finite, `w/h <= 0`) and hidden shapes are skipped.
2. **Match** each movable edge against the candidates within `tol`, keeping at most **two distinct
   winners per axis**, ordered lexicographically by `(prio, |d|)`.
3. **Offset** comes from the first winner (the nearest); the guides are drawn full-span.
4. **Spacing fallback** runs only on an axis with no alignment winner (below).

## Decisions and why

### Adjacency beats the slide on a tie (`(prio, |d|)`, not `|d|` alone)

When a box is equidistant from a neighbour's edge and the slide centre, it must snap to the
**neighbour**. The old match took the smallest `|d|` and, on an exact tie (strict `<`), the first
candidate won — and the slide's lines are pushed into the candidate list before the shapes, so the
slide always won the tie. Ranking by `(prio, |d|)` fixes it: shape adjacency is priority 0, a manual
guide is 1, the slide is 2, so the universal magnet loses an exact tie. Distance still wins across
priorities (a 1 pt alignment beats a 4 pt adjacency).

### Up to two guides per axis, full-span

Alignment often lands on two lines at once (a box's left on one neighbour and its right on another).
Showing only the single best match hides half the information. `pick_wins` keeps up to two distinct
winners per axis. A second guide is emitted only when it is exact with the same offset
(`|d - d0| <= 1e-6`) — never a "lying" guide 0.5 pt off the border. Guides span the whole slide
(`from: 0, to: size.height` for vertical) like PowerPoint and Figma, not just the two boxes.

### Spacing is gap-equality, and only when nothing aligns

Spacing (equidistance) is not line-alignment; it is "these two gaps are equal". `spacing_shift_x`
sorts the boxes by `x0`, takes the moving box's immediate left and right neighbours, and moves the
box so `gap_left == gap_right`. Rules: only on an axis with **no** alignment winner (alignment
always outranks spacing), at most one spacing guide per axis, shape-to-shape only (the slide edge is
not a neighbour in v1), tolerance = `tol`. The gap is labelled in **pt** (`4.0 pt`), never screen px,
because px scale with zoom and would lie.

### Temporal hysteresis: sticky guides

A hand tremor of sub-pixel amplitude must not make the guide blink on and off. Snapping uses a
Schmitt trigger: it **enters** at `tol` but only **exits** at `tol * 1.6`, and the guide currently
held keeps priority against a marginally nearer line. `ToolState::snap_hold` remembers the held line
for the gesture and is reset when a new move starts or the drag ends.

## Lessons from elsewhere

- **GIMP MR !860** (`gimpimage-snap.c`): confirms the 2x 1-D split (`gimp_image_snap_x/y`), and its
  equidistance is the same gap-equality idea. Its gaps: a fixed `O(N*M)` pass with no hidden-shape
  filter, `break` on the first match (one guide per axis), a hand-rolled `RINT`/`(gint)` round that
  desyncs label from geometry, and no hysteresis (state cleared on `button_release`). DeckCraft
  skips hidden shapes, keeps multiple winners, and snaps in `f64` end to end.
- **VectorCraft #3701 / fix `970e62f`**: the "align" label appeared but the object stayed offset
  because the snap referenced the **pressed point**, not the object (`snap(cx, start, d)`); the fix
  snapped a `hold` point on the outline instead. DeckCraft's move snaps the **bounds + delta**, not
  the click point, so it does not have that bug — proven by
  `move_snaps_the_bounds_not_the_pressed_spot`. Its live relative: `bounds()` ignores stroke width,
  so the ink edge lands half a stroke off at high zoom.

## Performance

`snap_edges` sorts the per-axis candidates (`sort_by(total_cmp)`); above `SNAP_DIRECT_MAX` (4000) it
scans only a local `±tol*4` window via `partition_point` instead of every candidate, and reserves
capacity up front. Below the cap the direct loop is faster than a binary search. Same result, faster
on dense slides. **No R-tree, no spatial hash, no KD-tree** — see "The model" above for why they do
not fit axis-aligned snap; a spatial broadphase only earns its keep for rotated (OBB) snap, which is
deferred.

## Not done yet (out of scope for this pass)

- **Rotated snap (OBB).** `snap_resize` bails when `rot != 0` (no jump, but no snap); `drag_move`
  uses the axis-aligned bounds. Real OBB snap wants SAT + a spatial broadphase.
- **Stroke-aware bounds.** `bounds()` and `xfrm_of` ignore stroke width; a `snap_to_stroke` flag is
  the fix.
- **Viewport culling.** The engine does not know the viewport, so `snap_edges` compares against every
  shape, not just the visible ones. A `visible: Rect` parameter is the fix.
- **Snap for create / vertex / line-end drags** (only move and resize snap today).
- **The zoom-large crash is not this snap.** Snap is `O(N shapes)` and zoom-independent (`tol = 4/scale`
  only shrinks), so at worst the mousemove lags. The panic lives in the UI/render: a canvas texture
  above the GPU limit (`crates/ui-egui/src/textures.rs`, "a larger canvas texture panics in egui")
  and unbounded `f32` coordinates without viewport culling. Separate fix.
- **Slide edge as a spacing neighbour?** v1 is shape-to-shape only; decide with a usability test.
- **Distinct colour for spacing vs alignment** (both use `t.guide` today).
