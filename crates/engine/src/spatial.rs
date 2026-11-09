//! Dynamic spatial index (R-tree) over slide-space AABBs, for proximity and viewport culling.
//!
//! This is the broadphase layer under smart guides: [`SpatialIndex::within`] and
//! [`SpatialIndex::neighbors`] find candidate shapes by 2D proximity, [`SpatialIndex::cull`] drops
//! everything outside a viewport before drawing. The exact gap/margin arithmetic and the
//! axis-aligned snap stay on the 1D path; this index only narrows the candidate set.

use std::collections::HashMap;

use deckcraft_geom::{Affine, Rect, group_child_affine};
use deckcraft_model::{Shape, ShapeId, ShapeKind};
use rstar::{AABB, RTree, RTreeObject, SelectionFunction};

use crate::DocState;
use crate::cmd::xfrm_of;

/// One index entry: a shape's AABB in slide space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoxEntry {
    pub id: ShapeId,
    pub min: [f64; 2],
    pub max: [f64; 2],
}

impl BoxEntry {
    /// Builds an entry, normalising corners so `min <= max` on both axes.
    pub fn new(id: ShapeId, x0: f64, y0: f64, x1: f64, y1: f64) -> Self {
        Self { id, min: [x0.min(x1), y0.min(y1)], max: [x0.max(x1), y0.max(y1)] }
    }
    pub fn center(&self) -> [f64; 2] {
        [(self.min[0] + self.max[0]) / 2.0, (self.min[1] + self.max[1]) / 2.0]
    }
    pub fn width(&self) -> f64 {
        self.max[0] - self.min[0]
    }
    pub fn height(&self) -> f64 {
        self.max[1] - self.min[1]
    }
}

impl RTreeObject for BoxEntry {
    type Envelope = AABB<[f64; 2]>;

    fn envelope(&self) -> Self::Envelope {
        AABB::from_corners(self.min, self.max)
    }
}

/// A dynamic R-tree over shapes' AABBs, with the current box of every shape kept beside it so a
/// moved shape can be re-indexed and removed by id alone.
#[derive(Debug, Default)]
pub struct SpatialIndex {
    tree: RTree<BoxEntry>,
    entries: HashMap<ShapeId, BoxEntry>,
}

impl SpatialIndex {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn len(&self) -> usize {
        self.tree.size()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Inserts or moves a shape's box: the previously indexed box for the same id is dropped.
    pub fn upsert(&mut self, e: BoxEntry) {
        if let Some(old) = self.entries.insert(e.id, e) {
            self.tree.remove(&old);
        }
        self.tree.insert(e);
    }
    /// Removes the exact entry (id + current box). A stale box for the same id is left alone.
    pub fn remove(&mut self, e: &BoxEntry) {
        if self.entries.get(&e.id) == Some(e) {
            self.entries.remove(&e.id);
            self.tree.remove(e);
        }
    }
    /// Removes whatever box is indexed for `id`, returning it.
    pub fn remove_id(&mut self, id: ShapeId) -> Option<BoxEntry> {
        let old = self.entries.remove(&id)?;
        self.tree.remove(&old);
        Some(old)
    }
    /// Boxes intersecting a query rectangle (`min`/`max` in either corner order).
    pub fn within(&self, min: [f64; 2], max: [f64; 2]) -> Vec<BoxEntry> {
        self.tree.locate_in_envelope_intersecting(AABB::from_corners(min, max)).copied().collect()
    }
    /// Boxes within `tol` of `e`'s expanded box, `e` itself excluded: spacing candidates.
    pub fn neighbors(&self, e: &BoxEntry, tol: f64) -> Vec<BoxEntry> {
        let min = [e.min[0] - tol, e.min[1] - tol];
        let max = [e.max[0] + tol, e.max[1] + tol];
        self.within(min, max).into_iter().filter(|o| o.id != e.id).collect()
    }
    /// Viewport culling: only the boxes intersecting the region, pruning whole subtrees that miss
    /// it (the `SelectionFunction` broadphase).
    pub fn cull(&self, min: [f64; 2], max: [f64; 2]) -> Vec<BoxEntry> {
        let q = AABB::from_corners(min, max);
        let sel = RegionSel { min: q.lower(), max: q.upper() };
        self.tree.locate_with_selection_function(sel).copied().collect()
    }
}

/// Selection function for [`SpatialIndex::cull`]: unpack only nodes that touch the region.
#[derive(Clone, Copy, Debug)]
struct RegionSel {
    min: [f64; 2],
    max: [f64; 2],
}

impl RegionSel {
    fn intersects(&self, min: [f64; 2], max: [f64; 2]) -> bool {
        !(max[0] < self.min[0] || min[0] > self.max[0] || max[1] < self.min[1] || min[1] > self.max[1])
    }
}

impl SelectionFunction<BoxEntry> for RegionSel {
    fn should_unpack_parent(&self, envelope: &AABB<[f64; 2]>) -> bool {
        self.intersects(envelope.lower(), envelope.upper())
    }
    fn should_unpack_leaf(&self, leaf: &BoxEntry) -> bool {
        self.intersects(leaf.min, leaf.max)
    }
}

/// Flattens the edited shape tree (nested Groups included) into slide-space AABBs.
///
/// A group's children store their boxes in the group's child coordinate space; their index boxes
/// must be where they are drawn, so the walk carries the accumulated affine down the tree exactly
/// as the renderer does (`group_child_affine`). Non-finite boxes are skipped, like the renderer.
pub fn collect_boxes(st: &DocState) -> Vec<BoxEntry> {
    let mut out = Vec::new();
    for sh in st.shapes() {
        push_shape(st, sh, Affine::IDENTITY, &mut out);
    }
    out
}

fn push_shape(st: &DocState, sh: &Shape, parent: Affine, out: &mut Vec<BoxEntry>) {
    let x = xfrm_of(&st.doc, &st.selection, sh);
    if !x.is_finite() {
        return;
    }
    let rect = (parent * x.affine()).transform_rect_bbox(Rect::new(0.0, 0.0, x.w, x.h));
    if [rect.x0, rect.y0, rect.x1, rect.y1].iter().all(|v| v.is_finite()) {
        out.push(BoxEntry::new(sh.id, rect.x0, rect.y0, rect.x1, rect.y1));
    }
    if let ShapeKind::Group { children, child } = &sh.kind {
        let ch = Rect::new(child.x, child.y, child.x + child.w, child.y + child.h);
        let child_to_slide = parent * group_child_affine(&x, ch);
        for c in children {
            push_shape(st, c, child_to_slide, out);
        }
    }
}

/// Builds a fresh index over every shape on the edited page, groups flattened.
pub fn build_index(st: &DocState) -> SpatialIndex {
    let mut ix = SpatialIndex::new();
    for b in collect_boxes(st) {
        ix.upsert(b);
    }
    ix
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: u32, x0: f64, y0: f64, x1: f64, y1: f64) -> BoxEntry {
        BoxEntry::new(ShapeId(id), x0, y0, x1, y1)
    }

    #[test]
    fn spatial_index_inserts_queries_and_removes() {
        let mut ix = SpatialIndex::new();
        ix.upsert(e(1, 0.0, 0.0, 10.0, 10.0));
        ix.upsert(e(2, 20.0, 0.0, 30.0, 10.0));
        ix.upsert(e(3, 100.0, 100.0, 110.0, 110.0));
        assert_eq!(ix.len(), 3);
        // Within tol=15 of box 1: only box 2, never box 3.
        let n = ix.neighbors(&e(1, 0.0, 0.0, 10.0, 10.0), 15.0);
        let mut ids: Vec<u32> = n.iter().map(|b| b.id.0).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![2]);
        // A query rectangle around the first two boxes.
        let c = ix.within([0.0, 0.0], [35.0, 12.0]);
        assert_eq!(c.len(), 2);
        // Remove drops the entry for good.
        ix.remove(&e(2, 20.0, 0.0, 30.0, 10.0));
        assert_eq!(ix.len(), 2);
        assert!(ix.within([0.0, 0.0], [35.0, 12.0]).iter().all(|b| b.id != ShapeId(2)));
    }

    #[test]
    fn upsert_moves_a_shape_by_id_and_remove_id_clears_it() {
        let mut ix = SpatialIndex::new();
        ix.upsert(e(1, 0.0, 0.0, 10.0, 10.0));
        ix.upsert(e(1, 500.0, 500.0, 510.0, 510.0));
        assert_eq!(ix.len(), 1, "upsert replaces, never duplicates");
        assert!(ix.within([0.0, 0.0], [20.0, 20.0]).is_empty(), "old box is gone");
        assert_eq!(ix.within([490.0, 490.0], [520.0, 520.0]), vec![e(1, 500.0, 500.0, 510.0, 510.0)]);
        assert_eq!(ix.remove_id(ShapeId(1)), Some(e(1, 500.0, 500.0, 510.0, 510.0)));
        assert!(ix.is_empty());
    }

    #[test]
    fn cull_returns_only_boxes_in_region() {
        let mut ix = SpatialIndex::new();
        ix.upsert(e(1, 0.0, 0.0, 10.0, 10.0));
        ix.upsert(e(2, 100.0, 100.0, 110.0, 110.0));
        let c = ix.cull([0.0, 0.0], [20.0, 20.0]);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].id, ShapeId(1));
        // Corner order does not matter.
        assert_eq!(ix.cull([20.0, 20.0], [0.0, 0.0]).len(), 1);
    }

    #[test]
    fn build_index_collects_nested_group_boxes() {
        use serde_json::json;

        let mut s = crate::tests::session();
        s.execute("slide.new", &json!({"layout": "blank"})).unwrap();
        let a = s.execute("shape.insert", &json!({"preset": "rect", "rect": [0, 0, 50, 50]})).unwrap()["id"].as_u64().unwrap();
        let b = s.execute("shape.insert", &json!({"preset": "rect", "rect": [100, 0, 50, 50]})).unwrap()["id"].as_u64().unwrap();
        let g = s.execute("arrange.group", &json!({"ids": [a, b]})).unwrap()["id"].as_u64().unwrap();

        let before = { collect_boxes(s.doc().unwrap()) };
        assert_eq!(before.len(), 3, "group + 2 children: {before:?}");
        let ix = build_index(s.doc().unwrap());
        assert_eq!(ix.len(), 3);

        // Children are indexed in slide space, not in the group's child space: moving the group
        // must move their boxes with it (their own xfrm does not change).
        s.execute("shape.move", &json!({"dx": 100.0, "dy": 20.0})).unwrap();
        let after = { collect_boxes(s.doc().unwrap()) };
        let box_of = |v: &[BoxEntry], id: u64| *v.iter().find(|e| e.id.0 as u64 == id).unwrap();
        for id in [a, b, g] {
            let (p, q) = (box_of(&before, id), box_of(&after, id));
            assert!((q.min[0] - p.min[0] - 100.0).abs() < 1e-9, "box {id} x moved: {p:?} -> {q:?}");
            assert!((q.min[1] - p.min[1] - 20.0).abs() < 1e-9, "box {id} y moved: {p:?} -> {q:?}");
        }
    }
}
