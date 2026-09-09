//! 坐标、矩形运算等工具函数。
//!
//! 纯逻辑模块，应保持跨平台兼容并配有单元测试。

/// 物理像素坐标系下的整数矩形（i32 坐标，与 Win32 `RECT` 对齐）。
///
/// 截图覆盖层全程使用物理像素坐标：截图按物理分辨率存储，
/// 选区物理坐标可直接用于图像裁剪，无需 DPI 换算（见 AGENTS.md 3.3 节）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// 左上角 x（物理像素）。
    pub x: i32,
    /// 左上角 y（物理像素）。
    pub y: i32,
    /// 宽度（物理像素）。
    pub width: u32,
    /// 高度（物理像素）。
    pub height: u32,
}

impl Rect {
    /// 由两个对角点构造矩形（支持任意拖动方向，自动归一化）。
    pub fn from_points(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        let min_x = x0.min(x1);
        let max_x = x0.max(x1);
        let min_y = y0.min(y1);
        let max_y = y0.max(y1);
        Self {
            x: min_x,
            y: min_y,
            width: (max_x - min_x) as u32,
            height: (max_y - min_y) as u32,
        }
    }

    /// 右边界（exclusive）。
    pub fn right(&self) -> i32 {
        self.x + self.width as i32
    }

    /// 下边界（exclusive）。
    pub fn bottom(&self) -> i32 {
        self.y + self.height as i32
    }

    /// 是否退化（零宽或零高）。
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// 把矩形整体平移后钳制在 `bounds` 内（选区不允许超出屏幕边界）。
    ///
    /// 返回钳制后的矩形；若 `self` 大于 `bounds`，缩小为 `bounds`。
    pub fn clamp(&self, bounds: &Rect) -> Rect {
        let width = self.width.min(bounds.width);
        let height = self.height.min(bounds.height);
        let x = self.x.clamp(bounds.x, bounds.right() - width as i32);
        let y = self.y.clamp(bounds.y, bounds.bottom() - height as i32);
        Rect {
            x,
            y,
            width,
            height,
        }
    }
}

/// 浮点矩形（`min` 含、`max` 不含的半开区间，与 egui `Rect` 的填充语义对齐）。
///
/// 供 UI 层做几何分解的纯逻辑载体（egui 类型仅在 Windows target 可用，
/// 分解逻辑放这里才能跨平台单测）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RectF {
    /// 左边界。
    pub min_x: f32,
    /// 上边界。
    pub min_y: f32,
    /// 右边界（exclusive）。
    pub max_x: f32,
    /// 下边界（exclusive）。
    pub max_y: f32,
}

impl RectF {
    /// 由左右上下边界构造。
    pub fn new(min_x: f32, min_y: f32, max_x: f32, max_y: f32) -> Self {
        Self {
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    /// 宽。
    pub fn width(&self) -> f32 {
        self.max_x - self.min_x
    }

    /// 高。
    pub fn height(&self) -> f32 {
        self.max_y - self.min_y
    }

    /// 是否为正面积（宽和高均大于 0）。
    pub fn is_positive(&self) -> bool {
        self.width() > 0.0 && self.height() > 0.0
    }

    /// 是否与另一矩形有正面积重叠。
    pub fn intersects(&self, other: &RectF) -> bool {
        self.min_x < other.max_x
            && other.min_x < self.max_x
            && self.min_y < other.max_y
            && other.min_y < self.max_y
    }

    /// 交集（不相交时返回退化矩形）。
    pub fn intersect(&self, other: &RectF) -> RectF {
        RectF::new(
            self.min_x.max(other.min_x),
            self.min_y.max(other.min_y),
            self.max_x.min(other.max_x),
            self.max_y.min(other.max_y),
        )
    }

    /// 点是否在矩形内（半开区间 `[min, max)`）。
    pub fn contains(&self, p: (f32, f32)) -> bool {
        p.0 >= self.min_x && p.0 < self.max_x && p.1 >= self.min_y && p.1 < self.max_y
    }
}

/// 「块减圆角洞」分解出的一块可填充区域。
#[derive(Debug, Clone, PartialEq)]
pub enum HolePiece {
    /// 实心矩形块。
    Rect(RectF),
    /// 四角「方块减四分之一圆」月牙（闭合多边形顶点，扇形三角化填充）。
    Crescent(Vec<(f32, f32)>),
}

/// 把 `block` 挖去圆角矩形 `hole`（圆角半径 `radius`）后的区域，
/// 分解为一组**互不重叠**的可填充块（矩形 + 四角月牙多边形）。
///
/// 分解方式（`h = hole ∩ block`，`r` 钳制到 `h` 半宽半高以内）：
/// 上/下两条全宽条带 + 左/右两条**纵贯洞中缝全高**的矩形 +
/// 四个「r×r 角方块减四分之一圆」月牙。
///
/// 左右矩形必须纵贯中缝全高：若纵向内收 r，角部行（洞顶/底各 r 高）的
/// 左右两侧会漏出未遮挡条带（2026-08-23 工具条亮带 bug 的根因）。
pub fn block_minus_rounded_hole(block: &RectF, hole: &RectF, radius: f32) -> Vec<HolePiece> {
    if !block.intersects(hole) {
        return vec![HolePiece::Rect(*block)];
    }
    let h = block.intersect(hole);
    if !h.is_positive() {
        return vec![HolePiece::Rect(*block)];
    }
    let r = radius.min(h.width() * 0.5).min(h.height() * 0.5);
    let (l, t, rt, b) = (h.min_x, h.min_y, h.max_x, h.max_y);

    let mut pieces = Vec::with_capacity(8);
    let mut push_rect = |min_x: f32, min_y: f32, max_x: f32, max_y: f32| {
        let rect = RectF::new(min_x, min_y, max_x, max_y);
        if rect.is_positive() {
            pieces.push(HolePiece::Rect(rect));
        }
    };
    // 上 / 下全宽条带
    push_rect(block.min_x, block.min_y, block.max_x, t);
    push_rect(block.min_x, b, block.max_x, block.max_y);
    // 左 / 右矩形：纵贯洞中缝全高 [t, b]，与月牙块仅共边、不重叠
    push_rect(block.min_x, t, l, b);
    push_rect(rt, t, block.max_x, b);

    // 四角月牙：(px,py) = 洞角点，(dx,dy) = 从角指向洞中心的方向。
    // 月牙是凹多边形，但其所有边界点从洞外角可见（星形域），
    // 以洞外角为扇心的 fan 三角化恰好正确，可直接扇形填充。
    for &(px, py, dx, dy) in &[
        (l, t, 1.0f32, 1.0f32),  // 左上
        (rt, t, -1.0, 1.0),      // 右上
        (l, b, 1.0, -1.0),       // 左下
        (rt, b, -1.0, -1.0),     // 右下
    ] {
        let c = (px + dx * r, py + dy * r); // 圆角圆心
        let mut pts = vec![
            (px, py),          // 方形外角
            (px + dx * r, py), // 弧起点
        ];
        for step in [0.25f32, 0.5, 0.75] {
            // 弧方向向量：从 (0,-dy) 插值到 (-dx,0)
            let vx = -step * dx;
            let vy = -(1.0 - step) * dy;
            let len = (vx * vx + vy * vy).sqrt();
            pts.push((c.0 + vx / len * r, c.1 + vy / len * r));
        }
        pts.push((px, py + dy * r)); // 弧终点
        pieces.push(HolePiece::Crescent(pts));
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_points_normalizes_drag_direction() {
        // 反向拖动（右下 → 左上）也能得到正确的左上角矩形
        let r = Rect::from_points(100, 80, 20, 10);
        assert_eq!(r, Rect { x: 20, y: 10, width: 80, height: 70 });
    }

    #[test]
    fn from_points_zero_size() {
        let r = Rect::from_points(5, 5, 5, 5);
        assert!(r.is_empty());
    }

    #[test]
    fn bounds_helpers() {
        let r = Rect { x: 10, y: 20, width: 30, height: 40 };
        assert_eq!(r.right(), 40);
        assert_eq!(r.bottom(), 60);
        assert!(!r.is_empty());
    }

    #[test]
    fn clamp_keeps_inside_bounds() {
        let bounds = Rect { x: 0, y: 0, width: 100, height: 100 };
        // 右移出界 → 拉回
        let r = Rect { x: 80, y: 30, width: 30, height: 20 }.clamp(&bounds);
        assert_eq!(r, Rect { x: 70, y: 30, width: 30, height: 20 });
        // 完全越界
        let r = Rect { x: 200, y: -50, width: 10, height: 10 }.clamp(&bounds);
        assert_eq!(r, Rect { x: 90, y: 0, width: 10, height: 10 });
        // 比 bounds 还大 → 缩小到 bounds
        let r = Rect { x: -10, y: -10, width: 500, height: 500 }.clamp(&bounds);
        assert_eq!(r, bounds);
    }

    // ── block_minus_rounded_hole ─────────────────────────────────────────

    /// 射线法判断点是否在多边形内（月牙块用）。
    fn point_in_polygon(p: (f32, f32), poly: &[(f32, f32)]) -> bool {
        let (x, y) = p;
        let mut inside = false;
        let mut j = poly.len() - 1;
        for i in 0..poly.len() {
            let (xi, yi) = poly[i];
            let (xj, yj) = poly[j];
            if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
                inside = !inside;
            }
            j = i;
        }
        inside
    }

    /// 点是否被任一分解块覆盖。
    fn covered(pieces: &[HolePiece], p: (f32, f32)) -> bool {
        pieces.iter().any(|piece| match piece {
            HolePiece::Rect(r) => r.contains(p),
            HolePiece::Crescent(pts) => point_in_polygon(p, pts),
        })
    }

    /// 点是否在圆角矩形（精确圆弧，非多边形近似）内部。
    fn inside_rounded_rect(p: (f32, f32), min: (f32, f32), max: (f32, f32), r: f32) -> bool {
        let (x, y) = p;
        if x < min.0 || x >= max.0 || y < min.1 || y >= max.1 {
            return false;
        }
        // 四角：距圆心超过 r 的角部区域在洞外
        let cx = x.clamp(min.0 + r, max.0 - r);
        let cy = y.clamp(min.1 + r, max.1 - r);
        (x - cx) * (x - cx) + (y - cy) * (y - cy) < r * r
    }

    /// 回归测试（2026-08-23 亮带 bug）：洞的角部行（顶/底各 r 高）上，
    /// 洞左右两侧直到 block 边界的区域必须被覆盖。
    #[test]
    fn hole_corner_rows_are_covered_to_block_edges() {
        // 模拟实际场景：全屏 block + 底部工具条洞（含圆角）
        let block = RectF::new(0.0, 0.0, 1920.0, 1080.0);
        let hole = RectF::new(600.0, 900.0, 1320.0, 990.0);
        let r = 10.0;
        let pieces = block_minus_rounded_hole(&block, &hole, r);
        // 角部行（y 在 [900, 910) 与 (980, 990]）的左右两侧：曾漏遮的亮带
        for y in [905.0, 985.0] {
            assert!(covered(&pieces, (10.0, y)), "左侧角部行 y={y} 漏遮");
            assert!(covered(&pieces, (590.0, y)), "洞左边缘角部行 y={y} 漏遮");
            assert!(covered(&pieces, (1330.0, y)), "洞右边缘角部行 y={y} 漏遮");
            assert!(covered(&pieces, (1910.0, y)), "右侧角部行 y={y} 漏遮");
        }
    }

    /// 密集采样：block 内每一点「被分解覆盖」当且仅当「不在圆角洞内」，
    /// 且没有任何点被两块同时覆盖（半透明填充叠加会变深）。
    #[test]
    fn decomposition_is_exact_partition() {
        let block = RectF::new(0.0, 0.0, 500.0, 400.0);
        let hole = RectF::new(150.0, 120.0, 350.0, 260.0);
        let r = 16.0;
        let pieces = block_minus_rounded_hole(&block, &hole, r);
        // 多边形弧与精确圆的偏差 < 0.8px（22.5° 分段弦高），判定边界留 1px 死区；
        // 采样取像素中心（x.5），避开块间共边——边界归属由栅格化规则保证，
        // 半开区间与射线法在恰落于共边上的点存在定义性歧义，不纳入断言
        for iy in 2..398 {
            for ix in 2..498 {
                let p = (ix as f32 + 0.5, iy as f32 + 0.5);
                let in_hole = inside_rounded_rect(p, (150.0, 120.0), (350.0, 260.0), r);
                let in_hole_grow = inside_rounded_rect(p, (150.0, 120.0), (350.0, 260.0), r + 1.0);
                let cov = covered(&pieces, p);
                if in_hole == in_hole_grow {
                    assert_eq!(cov, !in_hole, "点 {p:?} 覆盖性错误");
                }
                // 无重叠：覆盖计数 ≤ 1
                let count = pieces
                    .iter()
                    .filter(|piece| match piece {
                        HolePiece::Rect(r) => r.contains(p),
                        HolePiece::Crescent(pts) => point_in_polygon(p, pts),
                    })
                    .count();
                assert!(count <= 1, "点 {p:?} 被 {count} 块重复覆盖");
            }
        }
    }

    /// 洞与块不相交 / 洞贴块边缘（部分在外）时的行为。
    #[test]
    fn hole_outside_or_clipped() {
        let block = RectF::new(0.0, 0.0, 100.0, 100.0);
        // 不相交 → 原样返回整块
        let away = RectF::new(200.0, 200.0, 300.0, 300.0);
        assert_eq!(
            block_minus_rounded_hole(&block, &away, 8.0),
            vec![HolePiece::Rect(block)]
        );
        // 洞右半在块外 → 按交集挖洞（h = [50,40]×[100,80]），块内区域仍被精确分割
        let clipped = RectF::new(50.0, 40.0, 150.0, 80.0);
        let pieces = block_minus_rounded_hole(&block, &clipped, 8.0);
        assert!(covered(&pieces, (10.5, 44.5))); // 角部行左侧（交集内）
        assert!(covered(&pieces, (10.5, 60.5))); // 洞左侧仍覆盖
        assert!(covered(&pieces, (60.5, 90.5))); // 洞下方仍覆盖
        assert!(!covered(&pieces, (70.5, 60.5))); // 洞内不覆盖
        assert!(!covered(&pieces, (95.5, 60.5))); // 洞被截断处（贴块右缘）同为洞内
    }
}
