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

    /// 面积（像素数）。
    pub fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
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
        assert_eq!(r.area(), 0);
    }

    #[test]
    fn bounds_helpers() {
        let r = Rect { x: 10, y: 20, width: 30, height: 40 };
        assert_eq!(r.right(), 40);
        assert_eq!(r.bottom(), 60);
        assert_eq!(r.area(), 1200);
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
}
