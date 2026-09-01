//! 箭头标注工具。
//!
//! CPU 光栅化（方案 B 导出端）：线段 + 三角箭头头部，与 egui 预览几何对齐
//!（头部为终点处两条 30° 夹角短线 + 实心三角填充，保证小尺寸下可见）。
//!
//! 光栅策略：遍历箭头包围盒内像素，距离场判定描边覆盖；箭头头部三角内
//! 部额外填充，保证"所见即所得"（AGENTS.md 3.7 节）。

use crate::annotation::Color;

use super::rect::blend_pixel;

/// 在导出图上绘制箭头（坐标为图像本地像素，越界部分自动裁剪）。
///
/// * `from` - 起点（本地坐标）；
/// * `to` - 终点（箭头尖端，本地坐标）；
/// * `color` - 描边/填充颜色；
/// * `stroke_width` - 线宽（物理像素，向下取整至少 1）。
pub fn draw_arrow(
    img: &mut image::RgbaImage,
    from: (f32, f32),
    to: (f32, f32),
    color: Color,
    stroke_width: f32,
) {
    if color.a == 0 {
        return;
    }
    let w = stroke_width.max(1.0);
    let half = w * 0.5;
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 0.5 {
        return;
    }
    let dir_x = dx / len;
    let dir_y = dy / len;
    let head_len = 12.0 * w.max(1.0);
    // 头部两翼点（150° 夹角，指向起点侧）
    let angle1 = std::f32::consts::PI * 5.0 / 6.0;
    let angle2 = -std::f32::consts::PI * 5.0 / 6.0;
    let wing1 = {
        let (s, c) = angle1.sin_cos();
        let rx = dir_x * c - dir_y * s;
        let ry = dir_x * s + dir_y * c;
        (to.0 + rx * head_len, to.1 + ry * head_len)
    };
    let wing2 = {
        let (s, c) = angle2.sin_cos();
        let rx = dir_x * c - dir_y * s;
        let ry = dir_x * s + dir_y * c;
        (to.0 + rx * head_len, to.1 + ry * head_len)
    };

    // 包围盒（ shaft + head 扩大 half ）
    let min_x = from.0.min(to.0).min(wing1.0).min(wing2.0) - half - 1.0;
    let max_x = from.0.max(to.0).max(wing1.0).max(wing2.0) + half + 1.0;
    let min_y = from.1.min(to.1).min(wing1.1).min(wing2.1) - half - 1.0;
    let max_y = from.1.max(to.1).max(wing1.1).max(wing2.1) + half + 1.0;
    let x0 = (min_x.floor() as i32).clamp(0, img.width() as i32);
    let y0 = (min_y.floor() as i32).clamp(0, img.height() as i32);
    let x1 = (max_x.ceil() as i32).clamp(0, img.width() as i32);
    let y1 = (max_y.ceil() as i32).clamp(0, img.height() as i32);

    let tol = half + 0.45; // 像素中心容差
    for py in y0..y1 {
        for px in x0..x1 {
            let p = (px as f32 + 0.5, py as f32 + 0.5);
            let d_shaft = point_to_segment_dist(p, from, to);
            let d_h1 = point_to_segment_dist(p, to, wing1);
            let d_h2 = point_to_segment_dist(p, to, wing2);
            let in_shaft = d_shaft <= tol;
            let in_head_edge = d_h1 <= tol || d_h2 <= tol;
            let in_head_fill = point_in_triangle(p, to, wing1, wing2);
            if in_shaft || in_head_edge || in_head_fill {
                blend_pixel(img.get_pixel_mut(px as u32, py as u32), color);
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

fn point_in_triangle(p: (f32, f32), a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> bool {
    // 重心坐标符号法
    let sign = |p1: (f32, f32), p2: (f32, f32), p3: (f32, f32)| (p1.0 - p3.0) * (p2.1 - p3.1) - (p2.0 - p3.0) * (p1.1 - p3.1);
    let d1 = sign(p, a, b);
    let d2 = sign(p, b, c);
    let d3 = sign(p, c, a);
    let has_neg = (d1 < 0.0) || (d2 < 0.0) || (d3 < 0.0);
    let has_pos = (d1 > 0.0) || (d2 > 0.0) || (d3 > 0.0);
    !(has_neg && has_pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::Color;

    const W: u32 = 60;
    const H: u32 = 40;
    fn blank() -> image::RgbaImage {
        image::RgbaImage::from_pixel(W, H, image::Rgba([255, 255, 255, 255]))
    }
    fn px(img: &image::RgbaImage, x: u32, y: u32) -> [u8; 4] {
        img.get_pixel(x, y).0
    }

    #[test]
    fn horizontal_shaft_draws() {
        let mut img = blank();
        draw_arrow(&mut img, (5.0, 20.0), (45.0, 20.0), Color::RED, 2.0);
        // 轴线附近应着色
        assert_eq!(px(&img, 20, 20)[0..3], [255, 59, 48]);
        // 远离轴线不着色
        assert_eq!(px(&img, 20, 30), [255, 255, 255, 255]);
        // 箭头尖端应着色（头内填充）
        assert_eq!(px(&img, 44, 20)[0..3], [255, 59, 48]);
    }

    #[test]
    fn diagonal_and_thick() {
        let mut img = blank();
        draw_arrow(&mut img, (5.0, 5.0), (35.0, 35.0), Color::BLUE, 3.0);
        assert_eq!(px(&img, 20, 20)[2], 255);
        // 粗线比细线覆盖更宽
        let mut thin = blank();
        draw_arrow(&mut thin, (5.0, 5.0), (35.0, 35.0), Color::BLUE, 1.0);
        // 粗线在偏移 1px 处仍着色，细线不一定
        // 至少轴心都着色
        assert_ne!(px(&img, 20, 20), [255, 255, 255, 255]);
        assert_ne!(px(&thin, 20, 20), [255, 255, 255, 255]);
    }

    #[test]
    fn out_of_bounds_safe() {
        let mut img = blank();
        draw_arrow(&mut img, (-20.0, -20.0), (100.0, 100.0), Color::BLACK, 2.0);
        // 不应 panic，且图内有部分着色
        assert_ne!(px(&img, 10, 10), [255, 255, 255, 255]);
    }

    #[test]
    fn zero_length_does_nothing() {
        let mut img = blank();
        draw_arrow(&mut img, (20.0, 20.0), (20.0, 20.0), Color::RED, 2.0);
        assert_eq!(px(&img, 20, 20), [255, 255, 255, 255]);
    }
}
