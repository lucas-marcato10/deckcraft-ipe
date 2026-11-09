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
}
