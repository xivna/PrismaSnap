//! 荧光笔 / 自由划线标注工具。
//!
//! CPU 光栅化（方案 B 导出端）：折线描边（圆头连接），荧光笔半透明叠加。
//! 预览与导出共用距离场判定：包围盒遍历 + 点到线段距离 ≤ half + 容差。

use crate::annotation::Color;

use super::rect::blend_pixel;

/// 绘制自由划线 / 荧光笔。
///
/// * `points` - 本地坐标折线点列（已按选区原点平移）；
/// * `color` / `stroke_width` - 颜色与线宽；
/// * `highlighter` - 荧光笔模式（半透明叠加，alpha 固定 110）。
pub fn draw_brush(
    img: &mut image::RgbaImage,
    points: &[(f32, f32)],
    color: Color,
    stroke_width: f32,
    highlighter: bool,
) {
    if points.len() < 2 || color.a == 0 {
        return;
    }
    let w = stroke_width.max(1.0);
    let half = w * 0.5;
    let draw_color = if highlighter {
        Color { r: color.r, g: color.g, b: color.b, a: 110 }
    } else {
        color
    };
    // 包围盒
    let mut min_x = points[0].0;
    let mut min_y = points[0].1;
    let mut max_x = min_x;
    let mut max_y = min_y;
    for &(x, y) in points.iter().skip(1) {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    let x0 = (min_x - half - 1.0).floor() as i32;
    let y0 = (min_y - half - 1.0).floor() as i32;
    let x1 = (max_x + half + 1.0).ceil() as i32;
    let y1 = (max_y + half + 1.0).ceil() as i32;
    let x0c = x0.clamp(0, img.width() as i32);
    let y0c = y0.clamp(0, img.height() as i32);
    let x1c = x1.clamp(0, img.width() as i32);
    let y1c = y1.clamp(0, img.height() as i32);
    let tol = half + 0.45;
    for py in y0c..y1c {
        for px in x0c..x1c {
            let p = (px as f32 + 0.5, py as f32 + 0.5);
            let mut hit = false;
            // 圆头：端点圆盘
            for &pt in &[points[0], points[points.len() - 1]] {
                let dx = p.0 - pt.0;
                let dy = p.1 - pt.1;
                if (dx * dx + dy * dy).sqrt() <= half + 0.45 {
                    hit = true;
                    break;
                }
            }
            if !hit {
                for w in points.windows(2) {
                    if point_to_segment_dist(p, w[0], w[1]) <= tol {
                        hit = true;
                        break;
                    }
                }
            }
            if hit {
                blend_pixel(img.get_pixel_mut(px as u32, py as u32), draw_color);
            }
        }
    }
}

fn point_to_segment_dist(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let abx = b.0 - a.0;
    let aby = b.1 - a.1;
    let apx = p.0 - a.0;
    let apy = p.1 - a.1;
    let ab2 = abx * abx + aby * aby;
    if ab2 < 1e-6 {
        return (apx * apx + apy * apy).sqrt();
    }
    let t = ((apx * abx + apy * aby) / ab2).clamp(0.0, 1.0);
    let cx = a.0 + t * abx;
    let cy = a.1 + t * aby;
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::Color;

    const W: u32 = 40;
    const H: u32 = 40;
    fn blank() -> image::RgbaImage {
        image::RgbaImage::from_pixel(W, H, image::Rgba([255, 255, 255, 255]))
    }
    fn px(img: &image::RgbaImage, x: u32, y: u32) -> [u8; 4] {
        img.get_pixel(x, y).0
    }

    #[test]
    fn horizontal_line_draws() {
        let mut img = blank();
        draw_brush(&mut img, &[(5.0, 20.0), (30.0, 20.0)], Color::RED, 2.0, false);
        assert_eq!(px(&img, 15, 20)[0..3], [255, 59, 48]);
        assert_eq!(px(&img, 15, 30), [255, 255, 255, 255]);
    }

    #[test]
    fn highlighter_is_semi_transparent() {
        let mut img = blank();
        draw_brush(&mut img, &[(5.0, 10.0), (30.0, 10.0)], Color::YELLOW, 6.0, true);
        let p = px(&img, 15, 10);
        // 黄色半透明叠在白底上 → 仍偏黄但未完全覆盖
        assert!(p[0] == 255);
        assert!(p[1] > 200 && p[1] < 255);
        assert!(p[2] < 150);
        assert_eq!(p[3], 255);
    }

    #[test]
    fn out_of_bounds_safe() {
        let mut img = blank();
        draw_brush(&mut img, &[(-10.0, -10.0), (50.0, 50.0)], Color::BLACK, 2.0, false);
        assert_ne!(px(&img, 10, 10), [255, 255, 255, 255]);
    }
}
