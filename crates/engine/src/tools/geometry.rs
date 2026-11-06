use super::hit::table_rows;
use super::{
    Drag, Guide, HistoryEntry, Point, Rect, Result, Session, Shape, ShapeId, ShapeKind, Spacing, ToolKind, Value, Vec2, Xfrm, cmd, json, xfrm_of,
};

/// Priority of a snap candidate: neighbouring shape (0) > manual guide (1) > slide (2).
/// Lowest wins, so on a `|d|` tie adjacency beats the slide's magnets (Bug B).
fn cand_prio(is_slide: bool, is_manual: bool) -> u8 {
    if is_slide {
        2
    } else if is_manual {
        1
    } else {
        0
    }
}

/// Which of a box's edges take part in the snap (left/centre/right, top/centre/bottom), and
/// whether equal-gap spacing applies to an axis with no alignment.
struct SnapEdges {
    xs_on: [bool; 3],
    ys_on: [bool; 3],
    spacing: bool,
}

/// Até 2 linhas distintas (|Δpos| > 1e-6) que continuam EXATAS depois do offset escolhido.
/// `hits` vem ordenado por `(prio, |d|)` — o primeiro é o vencedor do snap.
fn pick_wins(hits: &[(f64, f64, u8)]) -> Vec<(f64, f64, u8)> {
    let mut wins: Vec<(f64, f64, u8)> = vec![];
    for &(d, pos, pri) in hits {
        if wins.len() == 2 {
            break;
        }
        match wins.first() {
            None => wins.push((d, pos, pri)),
            // Só mostra a segunda linha se ela também fica exata com o MESMO offset;
            // uma guia a 0.5 pt da borda mente (goal: "não mentir").
            Some(&(best_d, _, _)) => {
                if (d - best_d).abs() <= 1e-6 && !wins.iter().any(|w| (w.1 - pos).abs() <= 1e-6) {
                    wins.push((d, pos, pri));
                }
            }
        }
    }
    wins
}

impl Session {
    /// End the freeform being drawn: create the shape (if it has at least two points).
    pub fn finish_freeform(&mut self, closed: bool) -> Result<Value> {
        let Some(Drag::Freeform { mode, points, .. }) = self.tool.drag.take() else { return Ok(Value::Null) };
        self.tool.path_preview.clear();
        self.tool.path_closing = false;
        if !self.tool.sticky {
            self.tool.kind = ToolKind::Select;
        }
        if points.len() < 2 {
            return Ok(Value::Null);
        }
        let pts: Vec<[f64; 2]> = points.iter().map(|q| [q.x, q.y]).collect();
        self.execute("shape.freeform", &json!({"points": pts, "closed": closed, "smooth": mode == "curve"}))
    }

    /// The connection site within glue range of `p` (excluding shape `exclude`).
    pub(super) fn glue_at(&self, p: Point, tol: f64) -> Option<(ShapeId, u32, Point)> {
        let st = self.active()?;
        crate::connect::nearest_site(&st.doc, &st.selection, st.shapes(), p, (tol * 2.0).max(6.0), None)
    }

    /// Show the sites of the shape near `p` and return the site an end at `p` would glue to.
    pub(super) fn update_sites(&mut self, p: Point, tol: f64, exclude: Option<ShapeId>) -> Option<(ShapeId, u32, Point)> {
        let (sites, glue) = match self.active() {
            Some(st) => {
                let shapes = st.shapes();
                let near = crate::connect::site_shape_at(&st.doc, &st.selection, shapes, p, tol * 3.0, exclude);
                let sites = near
                    .and_then(|id| shapes.iter().find(|s| s.id == id))
                    .map(|s| crate::connect::sites(&st.doc, &st.selection, s))
                    .unwrap_or_default();
                (sites, crate::connect::nearest_site(&st.doc, &st.selection, shapes, p, (tol * 2.0).max(6.0), exclude))
            }
            None => (vec![], None),
        };
        self.tool.sites = sites;
        self.tool.glue = glue.map(|g| g.2);
        glue
    }

    pub(super) fn set_xfrm(&mut self, id: ShapeId, x: Xfrm) -> Result<()> {
        // Rows scale from their drawn heights: a stored height is only a minimum (often 0).
        let drawn = self.active().and_then(|st| match &st.shape(id)?.kind {
            ShapeKind::Table(t) => Some(table_rows(st, t)),
            _ => None,
        });
        self.edit(|doc, sel| {
            let list = crate::shapes_mut(doc, sel).ok_or_else(|| cmd::bad("tool", "no slide"))?;
            if let Some(sh) = deckcraft_model::find_shape_mut(list, id) {
                sh.xfrm = Some(x);
                if let ShapeKind::Table(t) = &mut sh.kind {
                    // Tables resize their columns and rows proportionally.
                    let heights: Vec<f64> = match &drawn {
                        Some(d) if d.len() == t.rows.len() => d.clone(),
                        _ => t.rows.iter().map(|r| r.height).collect(),
                    };
                    let (tw, th) = (t.width().max(1e-6), heights.iter().map(|h| h.max(0.0)).sum::<f64>());
                    let kx = x.w / tw;
                    t.cols.iter_mut().for_each(|c| *c *= kx);
                    if th > 1e-6 {
                        let ky = x.h / th;
                        t.rows.iter_mut().zip(&heights).for_each(|(r, h)| r.height = h.max(0.0) * ky);
                    }
                }
            }
            Ok(())
        })
    }

    pub(super) fn set_xfrm_cmd(&mut self, id: ShapeId, x: Xfrm) -> Result<Value> {
        self.execute("shape.setBounds", &json!({"id": id, "x": x.x, "y": x.y, "w": x.w, "h": x.h}))?;
        if x.flip_h || x.flip_v {
            self.edit(|doc, sel| {
                if let Some(sh) = crate::shapes_mut(doc, sel).and_then(|l| deckcraft_model::find_shape_mut(l, id))
                    && let Some(xx) = sh.xfrm.as_mut()
                {
                    xx.flip_h = x.flip_h;
                    xx.flip_v = x.flip_v;
                }
                Ok(())
            })?;
        }
        Ok(Value::Null)
    }

    pub(super) fn refit_after_resize(&mut self, id: ShapeId) -> Result<()> {
        // A text box resized by hand keeps wrapping at its new width.
        let st = self.doc()?;
        if st.shape(id).is_some_and(|s| s.text_box) {
            let _ = cmd::text::fit_text_box(self, id);
        }
        Ok(())
    }

    pub(super) fn execute_dup_in_place(&mut self, ids: &[ShapeId]) -> Result<()> {
        self.edit(|doc, sel| {
            let list = cmd::current_shapes(doc, sel).cloned().unwrap_or_default();
            let mut copies: Vec<Shape> = list.iter().filter(|x| ids.contains(&x.id)).cloned().collect();
            let boxes: Vec<Xfrm> = copies.iter().map(|c| xfrm_of(doc, sel, c)).collect();
            cmd::edit::detach_on_slide(doc, sel, &mut copies);
            cmd::edit::reid("edit.duplicate", doc, &mut copies)?;
            for (c, b) in copies.iter_mut().zip(boxes) {
                c.xfrm = Some(b);
                c.ph = None;
            }
            let new_ids: Vec<ShapeId> = copies.iter().map(|c| c.id).collect();
            let shapes = crate::shapes_mut(doc, sel).ok_or_else(|| cmd::bad("duplicate", "no slide"))?;
            shapes.extend(copies);
            sel.shapes = new_ids;
            Ok(())
        })
    }

    /// The drawing guide within `tol` of `p` (only while guides are shown).
    pub fn guide_at(&self, p: Point, tol: f64) -> Option<usize> {
        if !self.tool.guides_visible {
            return None;
        }
        let st = self.active()?;
        st.doc
            .guides
            .iter()
            .enumerate()
            .map(|(i, g)| (i, if g.vertical { (p.x - g.pos).abs() } else { (p.y - g.pos).abs() }))
            .filter(|(_, d)| *d <= tol.max(2.0))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    }

    /// Smart guides: snap `r` edges/centres to the slide and other shapes. Returns the offset to
    /// apply, the guides to draw and the equal-gap measurements to label.
    pub(super) fn snap_rect(&self, r: Rect, exclude: &[ShapeId], tol: f64, enabled: bool) -> (Vec2, Vec<Guide>, Vec<Spacing>) {
        self.snap_edges(r, exclude, tol, enabled, SnapEdges { xs_on: [true; 3], ys_on: [true; 3], spacing: true })
    }

    /// Equal-gap spacing on X: the moved box `r` keeps the gap between its immediate left
    /// neighbour and that neighbour's own left neighbour (an A-B-C chain), or, sitting between
    /// two neighbours, splits the difference so both gaps match. `None` unless a candidate lands
    /// within `tol`. `at` is the row's centre, where the measuring line is drawn.
    fn spacing_shift_x(&self, r: Rect, exclude: &[ShapeId], tol: f64) -> Option<(f64, Spacing)> {
        let st = self.active()?;
        let mut segs: Vec<(f64, f64)> = vec![]; // (x0, x1) of the boxes sharing the row
        for sh in st.shapes() {
            if exclude.contains(&sh.id) || sh.hidden {
                continue;
            }
            let b = xfrm_of(&st.doc, &st.selection, sh).bounds();
            if b.width() < 1e-9 || b.height() < 1e-9 {
                continue;
            }
            // Only boxes that share the row: a gap measured against a box on another row would
            // draw its line over empty slide.
            if b.y1 < r.y0 - 1e-6 || b.y0 > r.y1 + 1e-6 {
                continue;
            }
            segs.push((b.x0, b.x1));
        }
        let left = segs.iter().filter(|s| s.1 <= r.x0 + 1e-6).max_by(|a, b| a.1.total_cmp(&b.1)).copied();
        let right = segs.iter().filter(|s| s.0 >= r.x1 - 1e-6).min_by(|a, b| a.0.total_cmp(&b.0)).copied();
        let mut cands: Vec<(f64, f64)> = vec![]; // (dx, gap)
        // A-B-C: continue the chain after a pair, keeping the pair's own gap.
        if let Some(l) = left
            && let Some(ll) = segs.iter().filter(|s| s.1 <= l.0 - 1e-6).max_by(|a, b| a.1.total_cmp(&b.1))
        {
            let gap = l.0 - ll.1;
            if gap > 1e-9 {
                cands.push((l.1 + gap - r.x0, gap));
            }
        }
        // C-B-A: the same chain, mirrored (the box sits left of a pair).
        if let Some(rr) = right
            && let Some(rrr) = segs.iter().filter(|s| s.0 >= rr.1 + 1e-6).min_by(|a, b| a.0.total_cmp(&b.0))
        {
            let gap = rrr.0 - rr.1;
            if gap > 1e-9 {
                cands.push((rr.0 - gap - r.width() - r.x0, gap));
            }
        }
        // Between two neighbours: equalize both gaps by moving half the difference.
        if let (Some(l), Some(rr)) = (left, right) {
            let (gap_l, gap_r) = (r.x0 - l.1, rr.0 - r.x1);
            if gap_l >= -1e-9 && gap_r >= -1e-9 && (gap_l + gap_r) / 2.0 > 1e-9 {
                cands.push(((gap_r - gap_l) / 2.0, (gap_l + gap_r) / 2.0));
            }
        }
        let (dx, gap) = cands.into_iter().filter(|(dx, _)| dx.abs() <= tol).min_by(|a, b| a.0.abs().total_cmp(&b.0.abs()))?;
        Some((dx, Spacing { vertical: true, a: r.x0 + dx - gap, b: r.x0 + dx, at: r.center().y, gap }))
    }

    /// `spacing_shift_x` on the other axis: `a`/`b`/`at` are y positions and the measuring line
    /// runs across the column.
    fn spacing_shift_y(&self, r: Rect, exclude: &[ShapeId], tol: f64) -> Option<(f64, Spacing)> {
        let st = self.active()?;
        let mut segs: Vec<(f64, f64)> = vec![]; // (y0, y1) of the boxes sharing the column
        for sh in st.shapes() {
            if exclude.contains(&sh.id) || sh.hidden {
                continue;
            }
            let b = xfrm_of(&st.doc, &st.selection, sh).bounds();
            if b.width() < 1e-9 || b.height() < 1e-9 {
                continue;
            }
            if b.x1 < r.x0 - 1e-6 || b.x0 > r.x1 + 1e-6 {
                continue;
            }
            segs.push((b.y0, b.y1));
        }
        let above = segs.iter().filter(|s| s.1 <= r.y0 + 1e-6).max_by(|a, b| a.1.total_cmp(&b.1)).copied();
        let below = segs.iter().filter(|s| s.0 >= r.y1 - 1e-6).min_by(|a, b| a.0.total_cmp(&b.0)).copied();
        let mut cands: Vec<(f64, f64)> = vec![]; // (dy, gap)
        if let Some(a) = above
            && let Some(aa) = segs.iter().filter(|s| s.1 <= a.0 - 1e-6).max_by(|x, y| x.1.total_cmp(&y.1))
        {
            let gap = a.0 - aa.1;
            if gap > 1e-9 {
                cands.push((a.1 + gap - r.y0, gap));
            }
        }
        if let Some(b) = below
            && let Some(bb) = segs.iter().filter(|s| s.0 >= b.1 + 1e-6).min_by(|x, y| x.0.total_cmp(&y.0))
        {
            let gap = bb.0 - b.1;
            if gap > 1e-9 {
                cands.push((b.0 - gap - r.height() - r.y0, gap));
            }
        }
        if let (Some(a), Some(b)) = (above, below) {
            let (gap_a, gap_b) = (r.y0 - a.1, b.0 - r.y1);
            if gap_a >= -1e-9 && gap_b >= -1e-9 && (gap_a + gap_b) / 2.0 > 1e-9 {
                cands.push(((gap_b - gap_a) / 2.0, (gap_a + gap_b) / 2.0));
            }
        }
        let (dy, gap) = cands.into_iter().filter(|(dy, _)| dy.abs() <= tol).min_by(|a, b| a.0.abs().total_cmp(&b.0.abs()))?;
        Some((dy, Spacing { vertical: false, a: r.y0 + dy - gap, b: r.y0 + dy, at: r.center().x, gap }))
    }

    /// `snap_rect` looking only at some of `r`'s edges: `xs_on` is left, centre, right and `ys_on`
    /// top, centre, bottom. Resizing passes just the edges being dragged, so a fixed edge that
    /// already sits on a line (distance 0) doesn't win and leave the dragged edge unsnapped.
    /// `spacing` is off for a resize: equal gaps only make sense for a whole box that moves.
    fn snap_edges(&self, r: Rect, exclude: &[ShapeId], tol: f64, enabled: bool, edges: SnapEdges) -> (Vec2, Vec<Guide>, Vec<Spacing>) {
        let SnapEdges { xs_on, ys_on, spacing } = edges;
        let Some(st) = self.active() else { return (Vec2::ZERO, vec![], vec![]) };
        if !enabled || !self.prefs.smart_guides {
            return (Vec2::ZERO, vec![], vec![]);
        }
        let size = st.doc.slide_size;
        let mut xs2: Vec<(f64, u8)> =
            vec![(0.0, cand_prio(true, false)), (size.width / 2.0, cand_prio(true, false)), (size.width, cand_prio(true, false))];
        let mut ys2: Vec<(f64, u8)> =
            vec![(0.0, cand_prio(true, false)), (size.height / 2.0, cand_prio(true, false)), (size.height, cand_prio(true, false))];
        if self.tool.guides_visible {
            for g in &st.doc.guides {
                if g.vertical {
                    xs2.push((g.pos, cand_prio(false, true)));
                } else {
                    ys2.push((g.pos, cand_prio(false, true)));
                }
            }
        }
        for sh in st.shapes() {
            if exclude.contains(&sh.id) || sh.hidden {
                continue;
            }
            let b = xfrm_of(&st.doc, &st.selection, sh).bounds();
            for x in [b.x0, b.center().x, b.x1] {
                xs2.push((x, cand_prio(false, false)));
            }
            for y in [b.y0, b.center().y, b.y1] {
                ys2.push((y, cand_prio(false, false)));
            }
        }
        let mut hits_x: Vec<(f64, f64, u8)> = vec![]; // (d, pos, prio)
        for edge in [r.x0, r.center().x, r.x1].into_iter().zip(xs_on).filter_map(|(e, on)| on.then_some(e)) {
            for (x, pri) in &xs2 {
                let d = *x - edge;
                if d.abs() <= tol {
                    hits_x.push((d, *x, *pri));
                }
            }
        }
        hits_x.sort_by(|a, b| a.2.cmp(&b.2).then(a.0.abs().total_cmp(&b.0.abs())));
        let wins_x = pick_wins(&hits_x);
        let mut hits_y: Vec<(f64, f64, u8)> = vec![]; // (d, pos, prio)
        for edge in [r.y0, r.center().y, r.y1].into_iter().zip(ys_on).filter_map(|(e, on)| on.then_some(e)) {
            for (y, pri) in &ys2 {
                let d = *y - edge;
                if d.abs() <= tol {
                    hits_y.push((d, *y, *pri));
                }
            }
        }
        hits_y.sort_by(|a, b| a.2.cmp(&b.2).then(a.0.abs().total_cmp(&b.0.abs())));
        let wins_y = pick_wins(&hits_y);
        let mut off = Vec2::ZERO;
        let mut guides = vec![];
        let mut spacings: Vec<Spacing> = vec![];
        if let Some(&(d, _, _)) = wins_x.first() {
            off.x = d;
            for &(_, pos, _) in &wins_x {
                guides.push(Guide { vertical: true, pos, from: 0.0, to: size.height });
            }
        } else if spacing && let Some((dx, sp)) = self.spacing_shift_x(r, exclude, tol) {
            // Equal gap, not an alignment: no full-span line, only the short measurement (the
            // render draws it from `spacing`).
            off.x = dx;
            spacings.push(sp);
        } else if self.prefs.snap_to_grid && xs_on.contains(&true) {
            let at = if xs_on[0] {
                r.x0
            } else if xs_on[2] {
                r.x1
            } else {
                r.center().x
            };
            off.x = deckcraft_geom::snap(at, self.prefs.grid_spacing) - at;
        }
        if let Some(&(d, _, _)) = wins_y.first() {
            off.y = d;
            for &(_, pos, _) in &wins_y {
                guides.push(Guide { vertical: false, pos, from: 0.0, to: size.width });
            }
        } else if spacing && let Some((dy, sp)) = self.spacing_shift_y(r, exclude, tol) {
            off.y = dy;
            spacings.push(sp);
        } else if self.prefs.snap_to_grid && ys_on.contains(&true) {
            let at = if ys_on[0] {
                r.y0
            } else if ys_on[2] {
                r.y1
            } else {
                r.center().y
            };
            off.y = deckcraft_geom::snap(at, self.prefs.grid_spacing) - at;
        }
        (off, guides, spacings)
    }

    /// Snap the edges that resize handle `handle` drags in the resized box `x` to the smart guides.
    pub(super) fn snap_resize(&mut self, x: Xfrm, handle: u8, exclude: &[ShapeId], tol: f64) -> Xfrm {
        if x.rot != 0.0 {
            self.tool.guides.clear();
            self.tool.spacing.clear();
            return x;
        }
        let r = x.rect();
        // Only the dragged edges move, so only they look for a line to snap to. The handles sit in
        // the box's unflipped frame: in a flipped box, also one that a drag past the opposite edge
        // just flipped, the dragged handle is on the other side.
        let (left, right) = (matches!(handle, 0 | 6 | 7), matches!(handle, 2..=4));
        let (top, bottom) = (matches!(handle, 0..=2), matches!(handle, 4..=6));
        let (moves_left, moves_right) = if x.flip_h { (right, left) } else { (left, right) };
        let (moves_top, moves_bottom) = if x.flip_v { (bottom, top) } else { (top, bottom) };
        let (off, guides, spacings) = self.snap_edges(
            r,
            exclude,
            tol * 1.5,
            true,
            SnapEdges { xs_on: [moves_left, false, moves_right], ys_on: [moves_top, false, moves_bottom], spacing: false },
        );
        let mut nx = x;
        if moves_left {
            nx.x += off.x;
            nx.w -= off.x;
        } else if moves_right {
            nx.w += off.x;
        }
        if moves_top {
            nx.y += off.y;
            nx.h -= off.y;
        } else if moves_bottom {
            nx.h += off.y;
        }
        nx.w = nx.w.max(0.0);
        nx.h = nx.h.max(0.0);
        self.tool.guides = guides;
        self.tool.spacing = spacings;
        nx
    }

    /// One ink sample. No device reading is full pen width (`1`), not the old placeholder `0.5`.
    pub(super) fn ink_sample(&self, p: Point) -> (f64, f64, f32) {
        let pressure = match self.tool.pressure {
            Some(v) if v.is_finite() => v.clamp(0.0, 1.0),
            _ => 1.0,
        };
        (p.x, p.y, pressure)
    }

    pub(super) fn add_ink(&mut self, points: Vec<(f64, f64, f32)>) -> Result<Value> {
        let ToolKind::Ink { mode, color, width } = self.tool.kind.clone() else { return Ok(Value::Null) };
        let stroke = deckcraft_model::InkStroke { points, color, width, highlighter: mode == "highlighter" };
        // Strokes join the slide's ink layer (one ink shape per slide).
        let existing = self.doc()?.shapes().iter().rev().find(|s| matches!(s.kind, ShapeKind::Ink { .. })).map(|s| s.id);
        self.edit(|doc, sel| {
            let id = match existing {
                Some(id) => id,
                None => {
                    let id = ShapeId(doc.alloc_id().ok_or_else(|| cmd::bad("ink.draw", cmd::NO_ID_SPACE))?);
                    let size = doc.slide_size;
                    let list = crate::shapes_mut(doc, sel).ok_or_else(|| cmd::bad("ink", "no slide"))?;
                    list.push(Shape {
                        id,
                        name: "Ink".into(),
                        xfrm: Some(Xfrm::new(0.0, 0.0, size.width, size.height)),
                        kind: ShapeKind::Ink { strokes: vec![] },
                        ..Default::default()
                    });
                    id
                }
            };
            let list = crate::shapes_mut(doc, sel).ok_or_else(|| cmd::bad("ink", "no slide"))?;
            if let Some(sh) = deckcraft_model::find_shape_mut(list, id)
                && let ShapeKind::Ink { strokes } = &mut sh.kind
            {
                strokes.push(stroke);
            }
            Ok(json!({"ink": id}))
        })
    }

    pub(super) fn erase_at(&mut self, p: Point, tol: f64) -> Result<Value> {
        let st = self.doc()?;
        let Some(ink) = st.shapes().iter().rev().find(|s| matches!(s.kind, ShapeKind::Ink { .. })).map(|s| s.id) else { return Ok(Value::Null) };
        let before = st.doc.clone();
        let sel_before = st.selection.clone();
        self.edit(|doc, sel| {
            let list = crate::shapes_mut(doc, sel).ok_or_else(|| cmd::bad("ink", "no slide"))?;
            if let Some(sh) = deckcraft_model::find_shape_mut(list, ink)
                && let ShapeKind::Ink { strokes } = &mut sh.kind
            {
                strokes.retain(|s| !s.points.iter().any(|(x, y, _)| (Point::new(*x, *y) - p).hypot() <= tol * 2.0 + s.width));
            }
            Ok(())
        })?;
        let vertex_kind = self.tool.vertex_kind.clone();
        if let Some(st) = self.active_mut() {
            crate::push_undo(st, HistoryEntry { label: "Erase".into(), doc: before, selection: sel_before, vertex_kind });
        }
        Ok(Value::Null)
    }
}
