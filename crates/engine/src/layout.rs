//! Auto Layout engine: repositions children by gap/padding/direction (reflow).

use deckcraft_model::LayoutDir;

/// One child for layout resolution: its own size plus its resolved position.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChildBox {
    pub w: f64,
    pub h: f64,
    pub x: f64,
    pub y: f64,
}

/// Flat reflow of a container: places `children` in a 1-D flow starting at `(ox, oy)` (the
/// container's content origin) with `gap`, `padding` [top, right, bottom, left] and `dir`.
/// Returns the container's hugged size (w, h).
pub fn reflow(children: &mut [ChildBox], gap: f64, pad: [f64; 4], dir: LayoutDir, ox: f64, oy: f64) -> (f64, f64) {
    let mut main = 0.0_f64; // main-axis extent so far, gaps included
    let mut cross = 0.0_f64; // largest cross-axis size
    let mut first = true;
    for c in children.iter_mut() {
        if !first {
            main += gap;
        }
        first = false;
        match dir {
            LayoutDir::Horizontal => {
                c.x = ox + pad[3] + main;
                c.y = oy + pad[0];
                main += c.w;
                cross = cross.max(c.h);
            }
            LayoutDir::Vertical => {
                c.x = ox + pad[3];
                c.y = oy + pad[0] + main;
                main += c.h;
                cross = cross.max(c.w);
            }
        }
    }
    // Hug: the container fits its children plus gaps plus padding.
    match dir {
        LayoutDir::Horizontal => (pad[3] + main + pad[1], pad[0] + cross + pad[2]),
        LayoutDir::Vertical => (pad[3] + cross + pad[1], pad[0] + main + pad[2]),
    }
}

/// One node of a layout tree: a leaf with a fixed size, or a container whose children flow.
#[derive(Clone, Debug, PartialEq)]
pub enum LayoutNode {
    Leaf { w: f64, h: f64 },
    Container { gap: f64, pad: [f64; 4], dir: LayoutDir, children: Vec<LayoutNode> },
}

/// A node placed by [`reflow_tree`]: its box in its parent's space, and its own placed children.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutBox {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub children: Vec<LayoutBox>,
}

/// A layout node's size (hug), measured recursively bottom-up: a nested container is sized from
/// its own children before its parent flows it.
pub fn measure(node: &LayoutNode) -> (f64, f64) {
    match node {
        LayoutNode::Leaf { w, h } => (*w, *h),
        LayoutNode::Container { gap, pad, dir, children } => {
            let mut boxes: Vec<ChildBox> = children.iter().map(|ch| {
                let (w, h) = measure(ch);
                ChildBox { w, h, x: 0.0, y: 0.0 }
            }).collect();
            reflow(&mut boxes, *gap, *pad, *dir, 0.0, 0.0)
        }
    }
}

/// Place a whole layout tree from `(ox, oy)`: every container flows its children (sized bottom-up
/// by [`measure`]) and each child container is placed in turn at its resolved origin.
pub fn reflow_tree(node: &LayoutNode, ox: f64, oy: f64) -> LayoutBox {
    match node {
        LayoutNode::Leaf { w, h } => LayoutBox { x: ox, y: oy, w: *w, h: *h, children: Vec::new() },
        LayoutNode::Container { gap, pad, dir, children } => {
            let mut boxes: Vec<ChildBox> = children.iter().map(|ch| {
                let (w, h) = measure(ch);
                ChildBox { w, h, x: 0.0, y: 0.0 }
            }).collect();
            let (w, h) = reflow(&mut boxes, *gap, *pad, *dir, ox, oy);
            let placed = children.iter().zip(boxes.iter()).map(|(ch, b)| reflow_tree(ch, b.x, b.y)).collect();
            LayoutBox { x: ox, y: oy, w, h, children: placed }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn c(w: f64, h: f64) -> ChildBox {
        ChildBox { w, h, x: 0.0, y: 0.0 }
    }
    #[test]
    fn reflow_horizontal_flows_and_hugs() {
        let mut kids = vec![c(100.0, 50.0), c(100.0, 50.0), c(100.0, 50.0)];
        let (w, h) = reflow(&mut kids, 20.0, [10.0; 4], LayoutDir::Horizontal, 0.0, 0.0);
        assert!((kids[0].x - 10.0).abs() < 1e-9 && (kids[1].x - 130.0).abs() < 1e-9 && (kids[2].x - 250.0).abs() < 1e-9);
        assert!((kids[0].y - 10.0).abs() < 1e-9);
        assert!((w - 360.0).abs() < 1e-9, "w={w}");
        assert!((h - 70.0).abs() < 1e-9, "h={h}");
    }
    #[test]
    fn reflow_vertical_flows_main_axis_is_y() {
        let mut kids = vec![c(30.0, 40.0), c(30.0, 40.0)];
        let (w, h) = reflow(&mut kids, 5.0, [0.0; 4], LayoutDir::Vertical, 0.0, 0.0);
        assert!((kids[1].y - 45.0).abs() < 1e-9, "y={}", kids[1].y);
        assert!((h - 85.0).abs() < 1e-9 && (w - 30.0).abs() < 1e-9);
    }
    #[test]
    fn nested_reflow_measures_bottom_up() {
        // Outer horizontal container, gap 0, padding 0: [inner vertical container (two 10×10
        // leaves, gap 0), 5×5 leaf]. Inner measures 10×20; outer is 10 + 5 wide, max(20, 5) tall.
        let inner = LayoutNode::Container {
            gap: 0.0,
            pad: [0.0; 4],
            dir: LayoutDir::Vertical,
            children: vec![LayoutNode::Leaf { w: 10.0, h: 10.0 }, LayoutNode::Leaf { w: 10.0, h: 10.0 }],
        };
        let root = LayoutNode::Container {
            gap: 0.0,
            pad: [0.0; 4],
            dir: LayoutDir::Horizontal,
            children: vec![inner, LayoutNode::Leaf { w: 5.0, h: 5.0 }],
        };
        let (w, h) = measure(&root);
        assert!((w - 15.0).abs() < 1e-9, "w={w}");
        assert!((h - 20.0).abs() < 1e-9, "h={h}");
        // Every node is placed, not just measured: the inner container at the origin, its leaves
        // stacked, and the 5×5 leaf after the inner container.
        let placed = reflow_tree(&root, 100.0, 100.0);
        assert!((placed.x - 100.0).abs() < 1e-9 && (placed.y - 100.0).abs() < 1e-9);
        assert!((placed.w - 15.0).abs() < 1e-9 && (placed.h - 20.0).abs() < 1e-9);
        let inner_box = &placed.children[0];
        assert!((inner_box.x - 100.0).abs() < 1e-9 && (inner_box.y - 100.0).abs() < 1e-9, "{inner_box:?}");
        assert!((inner_box.w - 10.0).abs() < 1e-9 && (inner_box.h - 20.0).abs() < 1e-9, "{inner_box:?}");
        assert!((inner_box.children[0].y - 100.0).abs() < 1e-9);
        assert!((inner_box.children[1].y - 110.0).abs() < 1e-9, "{:?}", inner_box.children[1]);
        let leaf = &placed.children[1];
        assert!((leaf.x - 110.0).abs() < 1e-9 && (leaf.y - 100.0).abs() < 1e-9, "{leaf:?}");
    }
}
