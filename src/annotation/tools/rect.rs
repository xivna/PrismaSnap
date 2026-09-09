//! 矩形选框标注工具。
//!
//! CPU 光栅化（方案 B 导出端）：粗描边矩形 = 上下左右四条填充条带，
//! 线条贴矩形边界**外侧**绘制，与 egui 预览的 `StrokeKind::Outside` 一致，
//! 保证"所见即所得"（AGENTS.md 3.7 节）。

use crate::annotation::Color;
use crate::utils::math::Rect;

/// 在导出图上绘制矩形描边（坐标为图像本地像素，越界部分自动裁剪）。
///
/// * `img` - 目标图（sRGB RGBA8）；
/// * `rect` - 矩形；
/// * `color` - 描边颜色（含 alpha，与底图像素 alpha 混合）；
/// * `stroke_width` - 线宽（像素，向下取整且至少 1）。
pub fn draw_rect(img: &mut image::RgbaImage, rect: Rect, color: Color, stroke_width: f32) {
    let w = (stroke_width as i32).max(1);
    // 四条边条带（描边在矩形外侧：上/下条带占 y ∈ [y-w, y) 与 [bottom, bottom+w)）
    let top = Rect { x: rect.x - w, y: rect.y - w, width: rect.width + 2 * w as u32, height: w as u32 };
    let bottom = Rect { x: rect.x - w, y: rect.bottom(), width: rect.width + 2 * w as u32, height: w as u32 };
    let left = Rect { x: rect.x - w, y: rect.y, width: w as u32, height: rect.height };
    let right = Rect { x: rect.right(), y: rect.y, width: w as u32, height: rect.height };
    for bar in [top, bottom, left, right] {
        fill_rect(img, &bar, color);
    }
}

/// 用 alpha 混合方式填充矩形区域（与底图像素 over 合成）。
pub(crate) fn fill_rect(img: &mut image::RgbaImage, rect: &Rect, color: Color) {
    if color.a == 0 {
        return;
    }
    // 裁剪到图像范围（负坐标 / 越界部分丢弃）
    let x0 = rect.x.clamp(0, img.width() as i32);
    let y0 = rect.y.clamp(0, img.height() as i32);
    let x1 = rect.right().clamp(0, img.width() as i32);
    let y1 = rect.bottom().clamp(0, img.height() as i32);
    for py in y0..y1 {
        for px in x0..x1 {
            blend_pixel(img.get_pixel_mut(px as u32, py as u32), color);
        }
    }
}

/// 单像素 over 合成：`out = src·a + dst·(1-a)`。
///
/// 输出 alpha 取两者较大值——截图底图恒不透明（a=255），标注叠加后仍应不透明。
pub(crate) fn blend_pixel(p: &mut image::Rgba<u8>, c: Color) {
    if c.a == 255 {
        *p = image::Rgba([c.r, c.g, c.b, 255]);
        return;
    }
    let a = c.a as u32;
    let inv = 255 - a;
    let mix = |s: u8, d: u8| ((s as u32 * a + d as u32 * inv) / 255) as u8;
    let d = p.0;
    *p = image::Rgba([
        mix(c.r, d[0]),
        mix(c.g, d[1]),
        mix(c.b, d[2]),
        d[3].max(c.a),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 40;
    const H: u32 = 40;

    fn blank() -> image::RgbaImage {
        image::RgbaImage::from_pixel(W, H, image::Rgba([255, 255, 255, 255]))
    }

    fn px(img: &image::RgbaImage, x: u32, y: u32) -> [u8; 4] {
        img.get_pixel(x, y).0
    }

    #[test]
    fn stroke_covers_border_not_interior() {
        let mut img = blank();
        let red = Color::rgb(0xFF, 0, 0);
        draw_rect(&mut img, Rect { x: 10, y: 10, width: 20, height: 10 }, red, 1.0);
        // 纯外侧语义（对齐 egui StrokeKind::Outside）：条带在边界外一圈，
        // 即上边 y ∈ [9,10)、下边 y ∈ [20,21)、左 x ∈ [9,10)、右 x ∈ [30,31)
        assert_eq!(px(&img, 15, 9)[0..3], [255, 0, 0]);
        assert_eq!(px(&img, 15, 20)[0..3], [255, 0, 0]);
        assert_eq!(px(&img, 9, 15)[0..3], [255, 0, 0]);
        assert_eq!(px(&img, 30, 15)[0..3], [255, 0, 0]);
        // 矩形边界与内部保持白
        assert_eq!(px(&img, 10, 10), [255, 255, 255, 255]);
        assert_eq!(px(&img, 15, 15), [255, 255, 255, 255]);
        // 远处不受影响
        assert_eq!(px(&img, 0, 0), [255, 255, 255, 255]);
    }

    #[test]
    fn thick_stroke_extends_outward() {
        let mut img = blank();
        let blue = Color::rgb(0, 0, 0xFF);
        draw_rect(&mut img, Rect { x: 10, y: 10, width: 20, height: 20 }, blue, 3.0);
        // 外侧 3px 条带全着色（如上边 y ∈ [7, 10)）
        for y in 7..10 {
            assert_eq!(px(&img, 15, y)[0..3], [0, 0, 255], "y={y}");
        }
        assert_eq!(px(&img, 15, 6), [255, 255, 255, 255]); // 条带外
        // 右侧条带 x ∈ [30, 33)
        assert_eq!(px(&img, 31, 15)[0..3], [0, 0, 255]);
        assert_eq!(px(&img, 33, 15), [255, 255, 255, 255]);
        // 内部不着色
        assert_eq!(px(&img, 20, 20), [255, 255, 255, 255]);
    }

    #[test]
    fn out_of_bounds_is_clipped_without_panic() {
        let mut img = blank();
        // 矩形 (-5,-5,20,20) 一半在图外：右条带 x ∈ [15,17)、下条带 y ∈ [15,17)
        draw_rect(&mut img, Rect { x: -5, y: -5, width: 20, height: 20 }, Color::GREEN, 2.0);
        // 角落 (0,0) 属于矩形内部，不着色
        assert_eq!(px(&img, 0, 0), [255, 255, 255, 255]);
        let g = px(&img, 8, 16); // 下条带可见段
        assert_eq!((g[0], g[1], g[2]), (52, 199, 89));
        let g2 = px(&img, 16, 8); // 右条带可见段
        assert_eq!((g2[0], g2[1], g2[2]), (52, 199, 89));
        // 完全出界也不 panic
        draw_rect(&mut img, Rect { x: -100, y: -100, width: 50, height: 50 }, Color::RED, 2.0);
        draw_rect(&mut img, Rect { x: 200, y: 200, width: 50, height: 50 }, Color::RED, 2.0);
    }

    #[test]
    fn semi_transparent_blends_over_background() {
        let mut img = blank();
        let half_red = Color { r: 255, g: 0, b: 0, a: 128 };
        draw_rect(&mut img, Rect { x: 10, y: 10, width: 10, height: 10 }, half_red, 1.0);
        let p = px(&img, 12, 9); // 上边条带 y ∈ [9,10)
        // 白底上叠 50% 红：(255+255)/2=255, (0+255)/2≈127
        assert_eq!(p[0], 255);
        assert_eq!(p[1], 127);
        assert_eq!(p[3], 255); // 底图不透明，输出仍不透明
    }
}
