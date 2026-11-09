//! Arrange: order, group, align, distribute, rotate and flip.

use deckcraft_geom::{Point, Rect, Xfrm};
use deckcraft_model::{AutoLayout, LayoutDir, Shape, ShapeId, ShapeKind};
use serde_json::{Value, json};

use super::{
    CommandSpec, NO_ID_SPACE, bad, bool_or, cmd, edit_geometry, has_doc, has_selection, has_slide, has_two, id_param, ok, str_param, targets,
    usize_param, with_param, xfrm_of,
};
use crate::{Result, Session};

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "guide.add",
            "Add Guide",
            ["View", "Show", "Guides"],
            None,
            "{vertical: bool, pos?: pt (default: the slide's centre)} → {index}. `defaults: true` adds the two centre guides when there are none",
            has_doc,
            guide_add
        ),
        cmd!("guide.move", "Move Guide", [], None, "{index, pos: pt}", has_doc, guide_move),
        cmd!("guide.delete", "Delete Guide", [], None, "{index}", has_doc, guide_delete),
        cmd!(query "guide.list", "Guides", [], None, "{} → [{vertical, pos}]", has_doc, guide_list),
        cmd!("arrange.bringToFront", "Bring to Front", ["Arrange"], Some("Cmd+Shift+]"), "{ids?}", has_selection, |s, p| order(s, p, 2)),
        cmd!("arrange.bringForward", "Bring Forward", ["Arrange"], Some("Cmd+]"), "{ids?}", has_selection, |s, p| order(s, p, 1)),
        cmd!("arrange.sendBackward", "Send Backward", ["Arrange"], Some("Cmd+["), "{ids?}", has_selection, |s, p| order(s, p, -1)),
        cmd!("arrange.sendToBack", "Send to Back", ["Arrange"], Some("Cmd+Shift+["), "{ids?}", has_selection, |s, p| order(s, p, -2)),
        cmd!(
            "arrange.reorder",
            "Reorder Objects",
            ["Arrange"],
            None,
            "{id, index: position among its siblings (0 = back; a group's child stays in its group)} → {index}",
            has_slide,
            reorder
        ),
        cmd!("arrange.group", "Group", ["Arrange"], Some("Cmd+Alt+G"), "{ids?}", has_two, group),
        cmd!("arrange.ungroup", "Ungroup", ["Arrange"], Some("Cmd+Alt+Shift+G"), "{ids?}", has_selection, ungroup),
        cmd!("arrange.regroup", "Regroup", ["Arrange"], Some("Cmd+Alt+J"), "{}", has_doc, regroup),
        cmd!(
            "arrange.autoLayout",
            "Auto Layout",
            ["Arrange"],
            None,
            "{gap?: pt (default 0), padding?: [top,right,bottom,left] pt or {top,right,bottom,left} pt (default 0), direction?: horizontal|vertical, on?: bool (false clears)} → {id, layout, ids}",
            has_selection,
            auto_layout
        ),
        cmd!(
            "arrange.align",
            "Align",
            ["Arrange", "Align or Distribute"],
            None,
            "{edge: left|center|right|top|middle|bottom, to?: slide|selection (default: slide for one object), ids?}",
            has_selection,
            align
        ),
        cmd!(
            "arrange.distribute",
            "Distribute",
            ["Arrange", "Align or Distribute"],
            None,
            "{dir: horizontal|vertical, to?: slide|selection, ids?}",
            has_selection,
            distribute
        ),
        cmd!("arrange.rotateLeft", "Rotate Left 90°", ["Arrange", "Rotate or Flip"], None, "{ids?}", has_selection, |s, p| s
            .execute("shape.rotate", &with_param(p, "by", json!(-90.0)))),
        cmd!("arrange.rotateRight", "Rotate Right 90°", ["Arrange", "Rotate or Flip"], None, "{ids?}", has_selection, |s, p| s
            .execute("shape.rotate", &with_param(p, "by", json!(90.0)))),
        cmd!("arrange.flipHorizontal", "Flip Horizontal", ["Arrange", "Rotate or Flip"], None, "{ids?}", has_selection, |s, p| s
            .execute("shape.flip", &with_param(p, "axis", json!("horizontal")))),
        cmd!("arrange.flipVertical", "Flip Vertical", ["Arrange", "Rotate or Flip"], None, "{ids?}", has_selection, |s, p| s
            .execute("shape.flip", &with_param(p, "axis", json!("vertical")))),
        cmd!("arrange.nudge", "Nudge", [], None, "{dx, dy, ids?} (arrow keys: 6 pt, with Alt 1 pt)", has_selection, |s, p| s
            .execute("shape.move", p)),
    ]
}

fn order(s: &mut Session, p: &Value, how: i32) -> Result<Value> {
    let ids = targets(s, p)?;
    // Already in place (front of its group, say): no change and no undo step.
    let mut trial = s.doc()?.shapes().to_vec();
    order_tree(&mut trial, &ids, how, 0);
    if stacking(&trial) == stacking(s.doc()?.shapes()) {
        return ok();
    }
    s.edit(|doc, sel| {
        let list = crate::shapes_mut(doc, sel).ok_or_else(|| bad("arrange", "no slide"))?;
        order_tree(list, &ids, how, 0);
        Ok(())
    })?;
    ok()
}

/// Every shape id with its depth, in tree order.
fn stacking(v: &[Shape]) -> Vec<(ShapeId, usize)> {
    let mut out = vec![];
    deckcraft_model::walk(v, &mut |sh, d| out.push((sh.id, d)));
    out
}

/// Restack `ids` among their siblings, in every list of the tree: a shape inside a group moves
/// within its group and never leaves it (PowerPoint's Selection pane arrows work the same way).
fn order_tree(list: &mut Vec<Shape>, ids: &[ShapeId], how: i32, depth: usize) {
    if depth > 64 {
        return;
    }
    if list.iter().any(|x| ids.contains(&x.id)) {
        order_list(list, ids, how);
    }
    for sh in list.iter_mut() {
        if let Some(children) = sh.children_mut() {
            order_tree(children, ids, how, depth + 1);
        }
    }
}

fn order_list(list: &mut Vec<Shape>, ids: &[ShapeId], how: i32) {
    match how {
        2 => {
            let (mut moved, rest): (Vec<Shape>, Vec<Shape>) = list.drain(..).partition(|x| ids.contains(&x.id));
            list.extend(rest);
            list.append(&mut moved);
        }
        -2 => {
            let (mut moved, rest): (Vec<Shape>, Vec<Shape>) = list.drain(..).partition(|x| ids.contains(&x.id));
            moved.extend(rest);
            *list = moved;
        }
        1 => {
            for i in (0..list.len().saturating_sub(1)).rev() {
                let (a, b) = (list.get(i).map(|x| ids.contains(&x.id)), list.get(i + 1).map(|x| ids.contains(&x.id)));
                if a == Some(true) && b == Some(false) {
                    list.swap(i, i + 1);
                }
            }
        }
        _ => {
            for i in 1..list.len() {
                let (a, b) = (list.get(i - 1).map(|x| ids.contains(&x.id)), list.get(i).map(|x| ids.contains(&x.id)));
                if a == Some(false) && b == Some(true) {
                    list.swap(i - 1, i);
                }
            }
        }
    }
}

/// Move one shape to `index` among its siblings (0 = back): the Selection pane's drag and drop.
/// A shape stays in the list that holds it, so a group's child stays in its group.
fn reorder(s: &mut Session, p: &Value) -> Result<Value> {
    let id = id_param(p, "id", "arrange.reorder")?.ok_or_else(|| bad("arrange.reorder", "missing `id`"))?;
    let to = usize_param(p, "index").ok_or_else(|| bad("arrange.reorder", "missing `index`"))?;
    let st = s.doc()?;
    let siblings: &[Shape] = match deckcraft_model::parent_of(st.shapes(), id) {
        None => return Err(bad("arrange.reorder", "no such shape")),
        Some(None) => st.shapes(),
        Some(Some(g)) => st.shape(g).map(Shape::children).unwrap_or(&[]),
    };
    let from = siblings.iter().position(|x| x.id == id).ok_or_else(|| bad("arrange.reorder", "no such shape"))?;
    if from == to.min(siblings.len().saturating_sub(1)) {
        return Ok(json!({"index": from}));
    }
    s.edit(|doc, sel| {
        let tree = crate::shapes_mut(doc, sel).ok_or_else(|| bad("arrange.reorder", "no slide"))?;
        let list = deckcraft_model::sibling_list_mut(tree, id).ok_or_else(|| bad("arrange.reorder", "no such shape"))?;
        let i = list.iter().position(|x| x.id == id).ok_or_else(|| bad("arrange.reorder", "no such shape"))?;
        let sh = list.remove(i);
        let at = to.min(list.len());
        list.insert(at, sh);
        Ok(json!({"index": at}))
    })
}

/// Bounds of shapes (rotation included).
pub(crate) fn union_bounds(xs: &[Xfrm]) -> Option<Rect> {
    let mut it = xs.iter().map(Xfrm::bounds);
    let first = it.next()?;
    Some(it.fold(first, |a, b| a.union(b)))
}

/// How deep groups may nest: as deep as the PPTX reader reads and the renderer draws. A saved
/// .deckcraft file stops opening at about 39 (the JSON reader's recursion limit), and walks over
/// the shape tree stop at 64.
pub const MAX_GROUP_DEPTH: usize = 32;

/// How many groups deep `shapes` nest (0 without groups).
fn group_depth(shapes: &[Shape]) -> usize {
    let mut d = 0;
    deckcraft_model::walk(shapes, &mut |sh, depth| {
        if sh.is_group() {
            d = d.max(depth + 1);
        }
    });
    d
}

fn group(s: &mut Session, p: &Value) -> Result<Value> {
    let ids = targets(s, p)?;
    s.edit(|doc, sel| {
        let current = super::current_shapes(doc, sel).cloned().unwrap_or_default();
        let mut members: Vec<Shape> = current.iter().filter(|x| ids.contains(&x.id)).cloned().collect();
        if members.len() < 2 {
            return Err(bad("arrange.group", "select two or more objects on the slide"));
        }
        if group_depth(&members) + 1 > MAX_GROUP_DEPTH {
            return Err(bad("arrange.group", format!("groups can be nested at most {MAX_GROUP_DEPTH} deep")));
        }
        for m in &mut members {
            if m.xfrm.is_none() {
                m.xfrm = Some(xfrm_of(doc, sel, m));
            }
            // Placeholders lose their link when grouped (PowerPoint keeps them out of groups).
            m.ph = None;
        }
        let xs: Vec<Xfrm> = members.iter().filter_map(|m| m.xfrm).collect();
        let b = union_bounds(&xs).ok_or_else(|| bad("arrange.group", "nothing to group"))?;
        let gx = Xfrm::new(b.x0, b.y0, b.width(), b.height());
        let id = ShapeId(doc.alloc_id().ok_or_else(|| bad("arrange.group", NO_ID_SPACE))?);
        let n = super::current_shapes(doc, sel).map(|v| v.len()).unwrap_or(0);
        let g = Shape {
            id,
            name: format!("Group {}", n + 1),
            xfrm: Some(gx),
            kind: ShapeKind::Group { children: members, child: gx },
            ..Default::default()
        };
        let list = crate::shapes_mut(doc, sel).ok_or_else(|| bad("arrange.group", "no slide"))?;
        let at = list.iter().rposition(|x| ids.contains(&x.id)).unwrap_or(list.len());
        list.insert(at + 1, g);
        list.retain(|x| !ids.contains(&x.id));
        sel.shapes = vec![id];
        Ok(json!({"id": id}))
    })
}

/// Auto Layout for the selected groups: set gap/padding/direction, or clear it with `on: false`.
/// [`Session::edit`] reflows the changed containers before the undo entry is committed, so the
/// children take their new places in the same step. Non-groups in the selection are ignored;
/// selecting no group at all is an error, since there is nothing to flow.
fn auto_layout(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "arrange.autoLayout";
    let setting = auto_layout_param(p)?;
    let ids = targets(s, p)?;
    let st = s.doc()?;
    let mut groups = vec![];
    let mut locked = 0;
    for id in &ids {
        let Some(sh) = st.shape(*id) else { continue };
        if !sh.is_group() {
            continue;
        }
        if sh.locked {
            locked += 1;
        } else {
            groups.push(*id);
        }
    }
    let Some(&first) = groups.first() else {
        return Err(bad(CMD, if locked > 0 { "the group is locked" } else { "select a group: Auto Layout arranges a group's objects" }));
    };
    s.edit(|doc, sel| {
        let shapes = crate::shapes_mut(doc, sel).ok_or_else(|| bad(CMD, "no slide"))?;
        let mut applied = Vec::with_capacity(groups.len());
        for id in &groups {
            if let Some(sh) = deckcraft_model::find_shape_mut(shapes, *id) {
                sh.layout = setting;
                applied.push(*id);
            }
        }
        if applied.is_empty() {
            return Err(bad(CMD, "no such shape"));
        }
        let id = applied.first().copied().unwrap_or(first);
        Ok(json!({"id": id, "layout": layout_json(setting), "ids": applied}))
    })
}

/// The Auto Layout the params ask for, or `None` to turn it off. Unspecified fields take the
/// defaults (no gap, no padding, horizontal), so every call sets a whole layout.
fn auto_layout_param(p: &Value) -> Result<Option<AutoLayout>> {
    const CMD: &str = "arrange.autoLayout";
    let on = match p.get("on") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(on)) => Some(*on),
        Some(_) => return Err(bad(CMD, "`on` is true or false")),
    };
    let gap = match p.get("gap") {
        None | Some(Value::Null) => 0.0,
        Some(v) => points(v, CMD, "gap")?,
    };
    let padding = padding_param(p)?.unwrap_or([0.0; 4]);
    let direction = match p.get("direction") {
        None | Some(Value::Null) => LayoutDir::Horizontal,
        Some(Value::String(s)) if s.eq_ignore_ascii_case("horizontal") => LayoutDir::Horizontal,
        Some(Value::String(s)) if s.eq_ignore_ascii_case("vertical") => LayoutDir::Vertical,
        Some(Value::String(s)) => return Err(bad(CMD, format!("`direction` is horizontal or vertical, not {s:?}"))),
        Some(_) => return Err(bad(CMD, "`direction` is \"horizontal\" or \"vertical\"")),
    };
    // `on: false` clears the layout; the other values were still checked above.
    if on == Some(false) {
        return Ok(None);
    }
    Ok(Some(AutoLayout { gap, padding, direction }))
}

/// A distance in points: a finite number, zero or more.
fn points(v: &Value, cmd: &str, key: &str) -> Result<f64> {
    let n = v.as_f64().ok_or_else(|| bad(cmd, format!("`{key}` is a number of points")))?;
    if !n.is_finite() || n < 0.0 {
        return Err(bad(cmd, format!("`{key}` is zero or more points")));
    }
    Ok(n)
}

/// `padding` as [top, right, bottom, left], from an array of four or an object of named sides.
fn padding_param(p: &Value) -> Result<Option<[f64; 4]>> {
    const CMD: &str = "arrange.autoLayout";
    let Some(v) = p.get("padding").filter(|v| !v.is_null()) else { return Ok(None) };
    if let Some(a) = v.as_array() {
        if a.len() != 4 {
            return Err(bad(CMD, "`padding` takes four numbers: [top, right, bottom, left]"));
        }
        let mut pad = [0.0; 4];
        for (slot, v) in pad.iter_mut().zip(a) {
            *slot = points(v, CMD, "padding")?;
        }
        return Ok(Some(pad));
    }
    if let Some(o) = v.as_object() {
        let mut pad = [0.0; 4];
        for (slot, key) in pad.iter_mut().zip(["top", "right", "bottom", "left"]) {
            let v = o.get(key).ok_or_else(|| bad(CMD, format!("`padding` is missing `{key}`")))?;
            *slot = points(v, CMD, "padding")?;
        }
        return Ok(Some(pad));
    }
    Err(bad(CMD, "`padding` takes [top, right, bottom, left] or {top, right, bottom, left}"))
}

/// The answer's layout, in the shape the params use; `null` when Auto Layout is off.
fn layout_json(l: Option<AutoLayout>) -> Value {
    match l {
        Some(l) => json!({
            "gap": l.gap,
            "padding": l.padding,
            "direction": match l.direction {
                LayoutDir::Horizontal => "horizontal",
                LayoutDir::Vertical => "vertical",
            },
        }),
        None => Value::Null,
    }
}

/// After an edit: refit every group whose children's geometry changed to the union of its
/// children (PowerPoint keeps a group's box on its children, so moving one child grows or
/// shrinks the group). The children keep their place on the slide: the group's box and its child
/// space change together. `old` is the presentation before the edit; only slides the edit
/// touched are looked at, and a group whose own box changed (it was moved or resized) is left to
/// that edit.
pub(crate) fn refit_groups(old: &deckcraft_model::Presentation, new: &mut deckcraft_model::Presentation) {
    // Each slide, master and layout is compared with its own earlier state, found by id: a move,
    // insertion or deletion shifts the indices, and shape ids repeat across slides.
    let old_slides: std::collections::HashMap<_, _> = old.slides.iter().map(|s| (s.id, s)).collect();
    for slide in new.slides.iter_mut() {
        let Some(before) = old_slides.get(&slide.id) else { continue };
        if std::sync::Arc::ptr_eq(before, slide) {
            continue;
        }
        let shapes = &mut std::sync::Arc::make_mut(slide).shapes;
        refit_list(Some(&before.shapes), shapes, 0);
    }
    // A child edited in Slide Master, layout, notes or handout view refits its group there too.
    for master in new.masters.iter_mut() {
        if let Some(before) = old.masters.iter().find(|m| m.id == master.id)
            && !std::sync::Arc::ptr_eq(before, master)
        {
            refit_master(before, std::sync::Arc::make_mut(master));
        }
    }
    for (before, after) in [(old.notes_master.as_ref(), new.notes_master.as_mut()), (old.handout_master.as_ref(), new.handout_master.as_mut())] {
        if let (Some(before), Some(after)) = (before, after)
            && !std::sync::Arc::ptr_eq(before, after)
        {
            refit_master(before, std::sync::Arc::make_mut(after));
        }
    }
}

fn refit_master(old: &deckcraft_model::Master, new: &mut deckcraft_model::Master) {
    refit_list(Some(&old.shapes), &mut new.shapes, 0);
    for layout in new.layouts.iter_mut() {
        refit_list(old.layouts.iter().find(|l| l.id == layout.id).map(|l| &l.shapes), &mut layout.shapes, 0);
    }
}

fn refit_list(old: Option<&Vec<Shape>>, list: &mut [Shape], depth: usize) {
    // Most edits touch a slide without groups: nothing to refit, and no per-shape lookup. Nesting
    // is bounded like the model's other walks.
    if depth > 64 || !list.iter().any(|sh| matches!(sh.kind, ShapeKind::Group { .. })) {
        return;
    }
    let by_id: std::collections::HashMap<ShapeId, &Shape> = old.map(|o| o.iter().map(|x| (x.id, x)).collect()).unwrap_or_default();
    for sh in list.iter_mut() {
        let before = by_id.get(&sh.id).copied();
        let old_children = before.and_then(|b| match &b.kind {
            ShapeKind::Group { children, .. } => Some(children),
            _ => None,
        });
        let own_before = before.map(|b| {
            (
                b.xfrm,
                match &b.kind {
                    ShapeKind::Group { child, .. } => Some(*child),
                    _ => None,
                },
            )
        });
        let smart = sh.diagram.is_some();
        // An Auto Layout container's box is its hugged content, set by the layout engine; the
        // union of its children would drop the padding.
        let auto = sh.layout.is_some();
        let ShapeKind::Group { children, child } = &mut sh.kind else { continue };
        refit_list(old_children, children, depth + 1);
        if smart || auto {
            // A SmartArt graphic's box is its own; its shapes are regenerated inside it.
            continue;
        }
        let (Some(old_children), Some((old_x, old_child))) = (old_children, own_before) else { continue };
        if old_x != sh.xfrm || old_child != Some(*child) || children.is_empty() {
            continue;
        }
        let same = old_children.len() == children.len() && old_children.iter().zip(children.iter()).all(|(a, b)| a.id == b.id && a.xfrm == b.xfrm);
        if same {
            continue;
        }
        let Some(group) = sh.xfrm else { continue };
        let xs: Vec<Xfrm> = children.iter().filter_map(|c| c.xfrm).collect();
        let Some(u) = union_bounds(&xs) else { continue };
        if ![u.x0, u.y0, u.x1, u.y1].iter().all(|v| v.is_finite()) {
            continue;
        }
        let union = Xfrm::new(u.x0, u.y0, u.width(), u.height());
        let placed = child_to_parent(&group, child, &union);
        sh.xfrm = Some(Xfrm { x: placed.x, y: placed.y, w: placed.w, h: placed.h, rot: group.rot, flip_h: group.flip_h, flip_v: group.flip_v });
        *child = union;
    }
}

/// A child's xfrm in slide space, given its group.
pub fn child_to_parent(group: &Xfrm, ch: &Xfrm, c: &Xfrm) -> Xfrm {
    let sx = if ch.w.abs() > 1e-9 { group.w / ch.w } else { 1.0 };
    let sy = if ch.h.abs() > 1e-9 { group.h / ch.h } else { 1.0 };
    let a = deckcraft_geom::group_child_affine(group, Rect::new(ch.x, ch.y, ch.x + ch.w, ch.y + ch.h));
    let center = a * Point::new(c.x + c.w / 2.0, c.y + c.h / 2.0);
    let (w, h) = (c.w * sx.abs(), c.h * sy.abs());
    let mut rot = c.rot + group.rot;
    let (mut fh, mut fv) = (c.flip_h, c.flip_v);
    if group.flip_h {
        fh = !fh;
        rot = -c.rot + group.rot;
    }
    if group.flip_v {
        fv = !fv;
        rot = -rot + 2.0 * group.rot;
    }
    Xfrm { x: center.x - w / 2.0, y: center.y - h / 2.0, w, h, rot: ((rot % 360.0) + 360.0) % 360.0, flip_h: fh, flip_v: fv }
}

fn ungroup(s: &mut Session, p: &Value) -> Result<Value> {
    let ids = targets(s, p)?;
    s.edit(|doc, sel| {
        let list = crate::shapes_mut(doc, sel).ok_or_else(|| bad("arrange.ungroup", "no slide"))?;
        let mut out = Vec::with_capacity(list.len());
        let mut new_sel = vec![];
        for sh in list.drain(..) {
            if ids.contains(&sh.id)
                && let (ShapeKind::Group { children, child }, Some(gx)) = (&sh.kind, sh.xfrm)
            {
                for c in children {
                    let mut c = c.clone();
                    if let Some(cx) = c.xfrm {
                        c.xfrm = Some(child_to_parent(&gx, child, &cx));
                    }
                    // Nested groups scale too: their child space stays, their box is mapped.
                    new_sel.push(c.id);
                    out.push(c);
                }
                continue;
            }
            out.push(sh);
        }
        *list = out;
        sel.shapes = new_sel.clone();
        Ok(json!({"ids": new_sel}))
    })
}

fn regroup(s: &mut Session, _p: &Value) -> Result<Value> {
    // Regroup the most recently ungrouped objects (from the undo history).
    let st = s.doc()?;
    let last = st.history.undo.iter().rev().find(|e| e.label == "Ungroup").map(|e| e.selection.shapes.clone());
    match last {
        Some(prev_group) => {
            // The ungroup's selection held the group id; its children are the current selection when nothing else changed.
            let _ = prev_group;
            let ids = s.doc()?.selection.shapes.clone();
            if ids.len() >= 2 { s.execute("arrange.group", &json!({"ids": ids})) } else { Err(bad("arrange.regroup", "nothing to regroup")) }
        }
        None => Err(bad("arrange.regroup", "nothing to regroup")),
    }
}

fn align(s: &mut Session, p: &Value) -> Result<Value> {
    let edge = str_param(p, "edge").unwrap_or("left").to_string();
    let ids = targets(s, p)?;
    let st = s.doc()?;
    let size = st.doc.slide_size;
    let boxes: Vec<(ShapeId, Xfrm)> = ids.iter().filter_map(|id| st.shape(*id).map(|x| (*id, xfrm_of(&st.doc, &st.selection, x)))).collect();
    let to_slide = match str_param(p, "to") {
        Some("slide") => true,
        Some(_) => false,
        None => boxes.len() < 2,
    };
    let reference = if to_slide {
        Rect::new(0.0, 0.0, size.width, size.height)
    } else {
        union_bounds(&boxes.iter().map(|b| b.1).collect::<Vec<_>>()).unwrap_or_default()
    };
    edit_geometry(s, p, "arrange.align", |sh| {
        if let Some(x) = sh.xfrm.as_mut() {
            let b = x.bounds();
            let (dx, dy) = match edge.as_str() {
                "left" => (reference.x0 - b.x0, 0.0),
                "center" => (reference.center().x - b.center().x, 0.0),
                "right" => (reference.x1 - b.x1, 0.0),
                "top" => (0.0, reference.y0 - b.y0),
                "middle" => (0.0, reference.center().y - b.center().y),
                "bottom" => (0.0, reference.y1 - b.y1),
                _ => (0.0, 0.0),
            };
            x.x += dx;
            x.y += dy;
        }
        Ok(())
    })
}

fn distribute(s: &mut Session, p: &Value) -> Result<Value> {
    let horiz = !str_param(p, "dir").unwrap_or("horizontal").starts_with('v');
    let ids = targets(s, p)?;
    let st = s.doc()?;
    let size = st.doc.slide_size;
    let mut boxes: Vec<(ShapeId, Rect)> =
        ids.iter().filter_map(|id| st.shape(*id).map(|x| (*id, xfrm_of(&st.doc, &st.selection, x).bounds()))).collect();
    let to_slide = str_param(p, "to") == Some("slide") || boxes.len() < 3;
    if boxes.is_empty() {
        return ok();
    }
    boxes.sort_by(|a, b| if horiz { a.1.x0.total_cmp(&b.1.x0) } else { a.1.y0.total_cmp(&b.1.y0) });
    let total: f64 = boxes.iter().map(|b| if horiz { b.1.width() } else { b.1.height() }).sum();
    let (start, end) = if to_slide {
        if horiz { (0.0, size.width) } else { (0.0, size.height) }
    } else {
        let lo = |f: fn(&Rect) -> f64| boxes.iter().map(|b| f(&b.1)).fold(f64::INFINITY, f64::min);
        let hi = |f: fn(&Rect) -> f64| boxes.iter().map(|b| f(&b.1)).fold(f64::NEG_INFINITY, f64::max);
        if horiz { (lo(|r| r.x0), hi(|r| r.x1)) } else { (lo(|r| r.y0), hi(|r| r.y1)) }
    };
    let n = boxes.len();
    let gap = if n > 1 { ((end - start) - total) / (n - 1) as f64 } else { 0.0 };
    let mut targets_pos: Vec<(ShapeId, f64)> = vec![];
    let mut cur = start;
    if n == 1 {
        let b = boxes.first().map(|b| b.1).unwrap_or_default();
        cur = start + ((end - start) - if horiz { b.width() } else { b.height() }) / 2.0;
    }
    for (id, b) in &boxes {
        let d = cur - if horiz { b.x0 } else { b.y0 };
        targets_pos.push((*id, d));
        cur += (if horiz { b.width() } else { b.height() }) + gap;
    }
    edit_geometry(s, p, "arrange.distribute", |sh| {
        if let (Some(x), Some((_, d))) = (sh.xfrm.as_mut(), targets_pos.iter().find(|(i, _)| *i == sh.id)) {
            if horiz {
                x.x += d;
            } else {
                x.y += d;
            }
        }
        Ok(())
    })
}

/// Guides are kept on the slide; a file can hold at most this many.
const MAX_GUIDES: usize = 200;

fn guide_limit(s: &Session, vertical: bool) -> Result<f64> {
    let size = s.doc()?.doc.slide_size;
    Ok(if vertical { size.width } else { size.height })
}

fn guide_add(s: &mut Session, p: &Value) -> Result<Value> {
    if bool_or(p, "defaults", false) {
        if !s.doc()?.doc.guides.is_empty() {
            return Ok(json!({"added": 0}));
        }
        let size = s.doc()?.doc.slide_size;
        return s.edit(|doc, _| {
            doc.guides = vec![
                deckcraft_model::DrawGuide { vertical: true, pos: size.width / 2.0 },
                deckcraft_model::DrawGuide { vertical: false, pos: size.height / 2.0 },
            ];
            Ok(json!({"added": 2}))
        });
    }
    let vertical = bool_or(p, "vertical", true);
    let limit = guide_limit(s, vertical)?;
    let pos = p.get("pos").and_then(Value::as_f64).filter(|v| v.is_finite()).unwrap_or(limit / 2.0).clamp(0.0, limit);
    if s.doc()?.doc.guides.len() >= MAX_GUIDES {
        return Err(bad("guide.add", "too many guides"));
    }
    s.edit(|doc, _| {
        doc.guides.push(deckcraft_model::DrawGuide { vertical, pos });
        Ok(json!({"index": doc.guides.len() - 1}))
    })
}

fn guide_index(s: &Session, p: &Value, cmd: &str) -> Result<usize> {
    let i = usize_param(p, "index").ok_or_else(|| bad(cmd, "missing `index`"))?;
    if i >= s.doc()?.doc.guides.len() {
        return Err(bad(cmd, "no such guide"));
    }
    Ok(i)
}

fn guide_move(s: &mut Session, p: &Value) -> Result<Value> {
    let i = guide_index(s, p, "guide.move")?;
    let vertical = s.doc()?.doc.guides.get(i).is_some_and(|g| g.vertical);
    let limit = guide_limit(s, vertical)?;
    let pos = p.get("pos").and_then(Value::as_f64).filter(|v| v.is_finite()).ok_or_else(|| bad("guide.move", "missing `pos`"))?.clamp(0.0, limit);
    s.edit(|doc, _| {
        if let Some(g) = doc.guides.get_mut(i) {
            g.pos = pos;
        }
        Ok(json!({"index": i, "pos": pos}))
    })
}

fn guide_delete(s: &mut Session, p: &Value) -> Result<Value> {
    let i = guide_index(s, p, "guide.delete")?;
    s.edit(|doc, _| {
        if i < doc.guides.len() {
            doc.guides.remove(i);
        }
        ok()
    })
}

fn guide_list(s: &mut Session, _p: &Value) -> Result<Value> {
    Ok(serde_json::to_value(&s.doc()?.doc.guides).unwrap_or_default())
}

#[cfg(test)]
#[path = "guides_tests.rs"]
mod guides_tests;
