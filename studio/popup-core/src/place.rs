use crate::caret::Caret;
use crate::Rect;

/// Gap between the caret and the popup, in logical pixels.
const GAP: f32 = 6.0;

/// A popup position in physical pixels, and the scale factor of the monitor
/// that holds it.
#[derive(Debug, Clone, Copy)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
    pub scale: f32,
}

/// Puts a popup of `width` x `height` logical pixels below the caret. If it
/// does not fit below, it goes above. It always stays inside the work area of
/// the caret's monitor.
pub fn below_caret(caret: &Caret, width: f32, height: f32) -> Placement {
    let (work, scale) = monitor_at(caret.rect.left, caret.rect.top);
    let p = compute(&caret.rect, &work, scale, width, height);
    crate::timing::mark(&format!(
        "place caret=({},{})-({},{}) popup=({},{}) scale={}",
        caret.rect.left, caret.rect.top, caret.rect.right, caret.rect.bottom, p.x, p.y, p.scale
    ));
    p
}

pub fn compute(caret: &Rect, work: &Rect, scale: f32, width: f32, height: f32) -> Placement {
    let w = (width * scale).round() as i32;
    let h = (height * scale).round() as i32;
    let gap = (GAP * scale).round() as i32;
    let x = caret.left.min(work.right - w).max(work.left);
    let mut y = caret.bottom + gap;
    if y + h > work.bottom {
        y = caret.top - gap - h;
    }
    let y = y.min(work.bottom - h).max(work.top);
    Placement { x, y, scale }
}

/// Work area and scale factor of the monitor nearest to a point.
pub fn monitor_at(x: i32, y: i32) -> (Rect, f32) {
    #[cfg(windows)]
    {
        crate::win::monitor_at(x, y)
    }
    #[cfg(not(windows))]
    {
        let _ = (x, y);
        (Rect { left: 0, top: 0, right: 1920, bottom: 1080 }, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: Rect = Rect { left: 0, top: 0, right: 1000, bottom: 800 };

    #[test]
    fn goes_below_when_it_fits() {
        let caret = Rect { left: 100, top: 100, right: 101, bottom: 120 };
        let p = compute(&caret, &WORK, 1.0, 300.0, 200.0);
        assert_eq!((p.x, p.y), (100, 126));
    }

    #[test]
    fn goes_above_near_the_bottom() {
        let caret = Rect { left: 100, top: 700, right: 101, bottom: 720 };
        let p = compute(&caret, &WORK, 1.0, 300.0, 200.0);
        assert_eq!((p.x, p.y), (100, 494));
    }

    #[test]
    fn stays_inside_on_the_right() {
        let caret = Rect { left: 950, top: 100, right: 951, bottom: 120 };
        let p = compute(&caret, &WORK, 2.0, 300.0, 200.0);
        assert_eq!((p.x, p.y), (400, 132));
    }
}
