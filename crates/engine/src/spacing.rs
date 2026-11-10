//! Smart-guide spacing on the horizontal axis: hierarchical equal gaps (R1) and symmetric
//! margins (R2).
//!
//! Every measurement is a horizontal distance between two box edges, drawn on a row (`at`).
//! Proximity is two-dimensional: the candidate set comes from the rstar spatial index, so a
//! neighbour 20 pt lower still counts — `snap_edges` only asks for spacing once alignment has
//! lost the X axis, so the hierarchy is alignment > spacing > grid.
//!
//! R1 — a box between two neighbours: the two gaps (left↔middle, middle↔right) are labelled
//! together and the box snaps where they are equal. R2 — an outer box nearing its container
//! edge: its margin to that edge matches the mirror box's margin to the opposite edge, and the
//! mirror's margin is labelled as the reference.

use crate::spatial::BoxEntry;
use crate::tools::Spacing;

/// How far (pt) above or below the dragged box another box may sit and still count as a spacing
/// neighbour. Smart guides work by proximity, not by a shared row: 24 pt is a third of an inch.
pub const PROXIMITY_Y: f64 = 24.0;

/// Tolerance of the box-edge comparisons: two edges closer than this are the same edge.
const EPS: f64 = 1e-6;

/// Horizontal gap between `a` and `b` with `b` on the right: `b.left - a.right` (negative when
/// the boxes overlap).
pub fn gap_x(a: &BoxEntry, b: &BoxEntry) -> f64 {
    b.min[0] - a.max[0]
}

/// Margin of `block` to the container's left content edge (`padding` shrinks the container).
pub fn margin_left(block: &BoxEntry, container: &BoxEntry, padding: f64) -> f64 {
    block.min[0] - container.min[0] - padding
}

/// Margin of `block` to the container's right content edge (`padding` shrinks the container).
pub fn margin_right(block: &BoxEntry, container: &BoxEntry, padding: f64) -> f64 {
    container.max[0] - padding - block.max[0]
}

/// R1: the dx that equalizes the gaps on both sides of `r`, plus the resulting gap. `None` when
/// `r` overlaps a neighbour (there is no gap to equalize) or the neighbours are back to back.
pub fn equal_gaps_x(r: &BoxEntry, left: &BoxEntry, right: &BoxEntry) -> Option<(f64, f64)> {
    let (gap_l, gap_r) = (gap_x(left, r), gap_x(r, right));
    if !gap_l.is_finite() || !gap_r.is_finite() || gap_l < 0.0 || gap_r < 0.0 {
        return None;
    }
    let gap = (gap_l + gap_r) / 2.0;
    if gap <= EPS {
        return None;
    }
    Some(((gap_r - gap_l) / 2.0, gap))
}

/// R2: the dx that makes `dragged`'s margin to its container edge equal `mirror`'s margin to the
/// opposite edge, plus that margin. `None` when both sit on the same side of the container (no
/// symmetry to draw) or the mirror hangs outside its content edge.
pub fn symmetric_margin_x(dragged: &BoxEntry, mirror: &BoxEntry, container: &BoxEntry, padding: f64) -> Option<(f64, f64)> {
    let mid = (container.min[0] + container.max[0]) / 2.0;
    let dragged_left = dragged.center()[0] < mid;
    if dragged_left == (mirror.center()[0] < mid) {
        return None;
    }
    let margin = if dragged_left { margin_right(mirror, container, padding) } else { margin_left(mirror, container, padding) };
    if !margin.is_finite() || margin < 0.0 {
        return None;
    }
    // Left: align to the content's left edge; right: align the box's right edge instead.
    let dx = if dragged_left { container.min[0] + padding + margin - dragged.min[0] } else { container.max[0] - padding - margin - dragged.max[0] };
    Some((dx, margin))
}

/// The boxes near `dragged` in 2-D: anywhere across the container's width, within
/// [`PROXIMITY_Y`] above or below it. A block a few points lower still takes part (no row
/// alignment required).
///
/// A linear sweep of `others` beats building an R-tree here: `nearby` runs on every mouse-move
/// during a drag, and at slide scale the tree build costs more than just scanning the boxes.
pub fn nearby(dragged: &BoxEntry, others: &[BoxEntry], container: &BoxEntry) -> Vec<BoxEntry> {
    let lo = container.min[0].min(dragged.min[0]);
    let hi = container.max[0].max(dragged.max[0]);
    let ylo = dragged.min[1] - PROXIMITY_Y;
    let yhi = dragged.max[1] + PROXIMITY_Y;
    others.iter().filter(|b| b.id != dragged.id && b.max[0] >= lo && b.min[0] <= hi && b.max[1] >= ylo && b.min[1] <= yhi).copied().collect()
}

/// The spacing decision for a horizontal move: the snap offset and the measurements to label.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    pub dx: f64,
    pub labels: Vec<Spacing>,
}

/// Hierarchical spacing on X over `others` in `container`:
///
/// 1. R1 — between two neighbours, both gaps are labelled and the box snaps where they match.
/// 2. A-B-C / C-B-A — continuing a pair's own gap on one side (the original chain behaviour).
/// 3. R2 — an outer box's margin equal to the mirror's opposite margin, both labelled, the
///    mirror's one marked as the reference.
///
/// A snap fires on the nearest target within `tol`; with no target that close, a box between two
/// neighbours still shows both live gaps so the equality can be seen coming.
pub fn plan_x(dragged: &BoxEntry, others: &[BoxEntry], container: &BoxEntry, padding: f64, tol: f64) -> Plan {
    if !tol.is_finite() || tol <= 0.0 || !dragged.min[0].is_finite() || !dragged.max[0].is_finite() || !dragged.min[1].is_finite() {
        return Plan::default();
    }
    let near = nearby(dragged, others, container);
    let at = dragged.center()[1];
    let left = near.iter().filter(|b| b.max[0] <= dragged.min[0] + EPS).max_by(|a, b| a.max[0].total_cmp(&b.max[0])).copied();
    let right = near.iter().filter(|b| b.min[0] >= dragged.max[0] - EPS).min_by(|a, b| a.min[0].total_cmp(&b.min[0])).copied();
    let mut cands: Vec<(f64, Vec<Spacing>)> = Vec::new(); // (dx, labels at the snapped position)

    // R1: sitting between two neighbours, both gaps have to match.
    if let (Some(l), Some(r)) = (left.as_ref(), right.as_ref())
        && let Some((dx, gap)) = equal_gaps_x(dragged, l, r)
    {
        let (x0, x1) = (dragged.min[0] + dx, dragged.max[0] + dx);
        cands.push((
            dx,
            vec![
                Spacing { vertical: true, a: l.max[0], b: x0, at, gap, reference: false },
                Spacing { vertical: true, a: x1, b: r.min[0], at, gap, reference: false },
            ],
        ));
    }
    // A-B-C: continue the gap of the pair on the left.
    if let Some(l) = left
        && let Some(ll) = near.iter().filter(|b| b.max[0] <= l.min[0] - EPS).max_by(|a, b| a.max[0].total_cmp(&b.max[0]))
    {
        let gap = gap_x(ll, &l);
        if gap > EPS {
            let (a, b) = (l.max[0], l.max[0] + gap);
            cands.push((b - dragged.min[0], vec![Spacing { vertical: true, a, b, at, gap, reference: false }]));
        }
    }
    // C-B-A: the same chain mirrored, matching the gap of the pair on the right.
    if let Some(r) = right
        && let Some(rr) = near.iter().filter(|b| b.min[0] >= r.max[0] + EPS).min_by(|a, b| a.min[0].total_cmp(&b.min[0]))
    {
        let gap = gap_x(&r, rr);
        if gap > EPS {
            let (a, b) = (r.min[0] - gap, r.min[0]);
            cands.push((a - dragged.max[0], vec![Spacing { vertical: true, a, b, at, gap, reference: false }]));
        }
    }
    // R2: an outer box takes the margin of the mirror on the opposite side of the container.
    let mid = (container.min[0] + container.max[0]) / 2.0;
    let dragged_left = dragged.center()[0] < mid;
    let outer =
        if dragged_left { !near.iter().any(|b| b.max[0] <= dragged.min[0] + EPS) } else { !near.iter().any(|b| b.min[0] >= dragged.max[0] - EPS) };
    if outer {
        for m in near.iter().filter(|b| (b.center()[0] < mid) != dragged_left) {
            let Some((dx, margin)) = symmetric_margin_x(dragged, m, container, padding) else { continue };
            let labels = if dragged_left {
                vec![
                    Spacing { vertical: true, a: container.min[0] + padding, b: dragged.min[0] + dx, at, gap: margin, reference: false },
                    Spacing { vertical: true, a: m.max[0], b: container.max[0] - padding, at: m.center()[1], gap: margin, reference: true },
                ]
            } else {
                vec![
                    Spacing { vertical: true, a: dragged.max[0] + dx, b: container.max[0] - padding, at, gap: margin, reference: false },
                    Spacing { vertical: true, a: container.min[0] + padding, b: m.min[0], at: m.center()[1], gap: margin, reference: true },
                ]
            };
            cands.push((dx, labels));
        }
    }
    // Nearest target wins; the sort is stable, so a tie keeps R1 before the chain and R2.
    cands.sort_by(|a, b| a.0.abs().total_cmp(&b.0.abs()));
    if let Some((dx, labels)) = cands.into_iter().find(|(dx, _)| dx.abs() <= tol) {
        return Plan { dx, labels };
    }
    // R1 without a snap yet: show both live gaps so the equality can be seen coming. Both have
    // to be real gaps — an overlap has no distance to label.
    if let (Some(l), Some(r)) = (left.as_ref(), right.as_ref()) {
        let (gap_l, gap_r) = (gap_x(l, dragged), gap_x(dragged, r));
        if gap_l >= 0.0 && gap_r >= 0.0 && gap_l + gap_r > EPS {
            return Plan {
                dx: 0.0,
                labels: vec![
                    Spacing { vertical: true, a: l.max[0], b: dragged.min[0], at, gap: gap_l, reference: false },
                    Spacing { vertical: true, a: dragged.max[0], b: r.min[0], at, gap: gap_r, reference: false },
                ],
            };
        }
    }
    Plan::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use deckcraft_model::ShapeId;

    fn b(id: u32, x0: f64, y0: f64, x1: f64, y1: f64) -> BoxEntry {
        BoxEntry::new(ShapeId(id), x0, y0, x1, y1)
    }

    #[test]
    fn gaps_and_margins_are_edge_distances() {
        let (a, m) = (b(1, 0.0, 0.0, 100.0, 80.0), b(2, 150.0, 0.0, 250.0, 80.0));
        assert!((gap_x(&a, &m) - 50.0).abs() < 1e-9);
        assert!((gap_x(&m, &a) + 250.0).abs() < 1e-9, "the edge distance in reverse order is negative");
        let c = b(3, 0.0, 0.0, 960.0, 540.0);
        assert!((margin_left(&m, &c, 0.0) - 150.0).abs() < 1e-9);
        assert!((margin_right(&m, &c, 0.0) - 710.0).abs() < 1e-9);
        assert!((margin_left(&m, &c, 10.0) - 140.0).abs() < 1e-9, "padding shrinks the content box");
    }

    #[test]
    fn equal_gaps_x_moves_half_the_difference() {
        // A=[0..100], M=[153..253], B=[300..400]: gaps 53 and 47 → dx -3, gap 50.
        let (a, m, b2) = (b(1, 0.0, 0.0, 100.0, 80.0), b(2, 153.0, 0.0, 253.0, 80.0), b(3, 300.0, 0.0, 400.0, 80.0));
        let (dx, gap) = equal_gaps_x(&m, &a, &b2).expect("two real gaps");
        assert!((dx + 3.0).abs() < 1e-9 && (gap - 50.0).abs() < 1e-9, "dx={dx} gap={gap}");
        // Overlapping a neighbour: no gap, no target.
        assert!(equal_gaps_x(&b(2, 60.0, 0.0, 160.0, 80.0), &a, &b2).is_none());
        // Back-to-back neighbours: nothing to equalize.
        assert!(equal_gaps_x(&m, &a, &b(3, 100.0, 0.0, 153.0, 80.0)).is_none());
    }

    #[test]
    fn symmetric_margin_matches_the_mirror_on_the_opposite_edge() {
        let c = b(9, 0.0, 0.0, 960.0, 540.0);
        let mirror = b(2, 860.0, 100.0, 910.0, 180.0); // 50 from the right edge
        let dragged = b(1, 46.0, 100.0, 146.0, 180.0); // left, 46 from the left edge
        let (dx, margin) = symmetric_margin_x(&dragged, &mirror, &c, 0.0).expect("opposite sides");
        assert!((dx - 4.0).abs() < 1e-9 && (margin - 50.0).abs() < 1e-9, "dx={dx} margin={margin}");
        // Right side: the box's right edge is what matches.
        let right = b(3, 800.0, 100.0, 900.0, 180.0);
        let (dx, margin) = symmetric_margin_x(&right, &b(4, 50.0, 100.0, 150.0, 180.0), &c, 0.0).expect("opposite sides");
        assert!((dx - 10.0).abs() < 1e-9 && (margin - 50.0).abs() < 1e-9, "dx={dx} margin={margin}");
        // Same side: there is no symmetry to draw.
        assert!(symmetric_margin_x(&dragged, &b(4, 50.0, 100.0, 150.0, 180.0), &c, 0.0).is_none());
    }

    #[test]
    fn plan_r1_labels_both_gaps_and_snaps_equal() {
        let c = b(9, 0.0, 0.0, 960.0, 540.0);
        let others = [b(1, 0.0, 100.0, 100.0, 180.0), b(3, 300.0, 100.0, 400.0, 180.0)];
        let dragged = b(2, 151.0, 100.0, 251.0, 180.0);
        let plan = plan_x(&dragged, &others, &c, 0.0, 4.5);
        assert!((plan.dx + 1.0).abs() < 1e-9, "dx={}", plan.dx);
        assert_eq!(plan.labels.len(), 2, "{:?}", plan.labels);
        for l in &plan.labels {
            assert!(l.vertical && !l.reference && (l.gap - 50.0).abs() < 1e-9, "{l:?}");
        }
        assert!(plan.labels.iter().any(|l| (l.a - 100.0).abs() < 1e-9 && (l.b - 150.0).abs() < 1e-9), "{:?}", plan.labels);
        assert!(plan.labels.iter().any(|l| (l.a - 250.0).abs() < 1e-9 && (l.b - 300.0).abs() < 1e-9), "{:?}", plan.labels);
    }

    #[test]
    fn plan_r1_live_gaps_when_not_yet_equal() {
        let c = b(9, 0.0, 0.0, 960.0, 540.0);
        let others = [b(1, 0.0, 100.0, 100.0, 180.0), b(3, 400.0, 100.0, 500.0, 180.0)];
        let dragged = b(2, 200.0, 100.0, 300.0, 180.0); // gaps 100/100 already; move off
        let plan = plan_x(&dragged, &others, &c, 0.0, 4.5);
        assert_eq!(plan.dx, 0.0);
        assert_eq!(plan.labels.len(), 2);
        // Far off: no snap, but both live gaps are still labelled.
        let dragged = b(2, 180.0, 100.0, 280.0, 180.0); // gaps 80/120
        let plan = plan_x(&dragged, &others, &c, 0.0, 4.5);
        assert_eq!(plan.dx, 0.0);
        let gaps: Vec<f64> = plan.labels.iter().map(|l| l.gap).collect();
        assert_eq!(gaps.len(), 2, "{:?}", plan.labels);
        assert!((gaps[0] - 80.0).abs() < 1e-9 && (gaps[1] - 120.0).abs() < 1e-9, "{gaps:?}");
    }

    #[test]
    fn plan_r2_labels_the_dragged_margin_and_the_mirror_reference() {
        let c = b(9, 0.0, 0.0, 960.0, 540.0);
        let others = [b(3, 860.0, 100.0, 910.0, 180.0)];
        let dragged = b(2, 48.0, 100.0, 148.0, 180.0);
        let plan = plan_x(&dragged, &others, &c, 0.0, 4.5);
        assert!((plan.dx - 2.0).abs() < 1e-9, "dx={}", plan.dx);
        assert_eq!(plan.labels.len(), 2, "{:?}", plan.labels);
        let dragged_label = plan.labels.iter().find(|l| !l.reference).expect("the dragged margin");
        assert!((dragged_label.a - 0.0).abs() < 1e-9 && (dragged_label.b - 50.0).abs() < 1e-9, "{dragged_label:?}");
        let reference = plan.labels.iter().find(|l| l.reference).expect("the mirror reference");
        assert!((reference.a - 910.0).abs() < 1e-9 && (reference.b - 960.0).abs() < 1e-9, "{reference:?}");
        assert!((reference.gap - 50.0).abs() < 1e-9 && (dragged_label.gap - 50.0).abs() < 1e-9);
    }

    #[test]
    fn plan_is_proximity_based_not_same_row() {
        let c = b(9, 0.0, 0.0, 960.0, 540.0);
        // A is 12 pt lower than the dragged box: still a neighbour (proximity, not a row).
        let others = [b(1, 0.0, 112.0, 100.0, 192.0), b(3, 300.0, 112.0, 400.0, 192.0)];
        let dragged = b(2, 151.0, 100.0, 251.0, 180.0);
        let plan = plan_x(&dragged, &others, &c, 0.0, 4.5);
        assert!((plan.dx + 1.0).abs() < 1e-9, "dx={}", plan.dx);
        assert_eq!(plan.labels.len(), 2);
        // A box 100 pt away shares no proximity band: no neighbours, no spacing.
        let far = [b(1, 0.0, 300.0, 100.0, 380.0), b(3, 300.0, 300.0, 400.0, 380.0)];
        assert_eq!(plan_x(&dragged, &far, &c, 0.0, 4.5), Plan::default());
    }

    #[test]
    fn plan_never_panics_on_junk() {
        let c = b(9, 0.0, 0.0, 960.0, 540.0);
        let junk = b(1, f64::NAN, 0.0, 100.0, 80.0);
        let dragged = b(2, f64::NAN, 0.0, 100.0, 80.0);
        assert_eq!(plan_x(&dragged, &[junk], &c, 0.0, 4.5), Plan::default());
        assert_eq!(plan_x(&b(2, 0.0, 0.0, 10.0, 10.0), &[], &c, 0.0, 0.0), Plan::default());
    }
}
