//! 箭头标注工具。
//!
//! 简约式样（2026-09-10 用户定稿）：线段 + 实心尖三角头（无描边填充，
//! 头长 3 倍线宽、内侧半角 22° 的瘦头；4w/25° 版显胖，已收）。此前开放 V 形
//! （预览/导出各一次，SDR 下尤其明显），改实心填充后缺口无从产生，
//! 预览（egui 实心多边形）与导出（CPU 三角形光栅化）天然一致。
//!
//! 光栅策略：遍历箭头包围盒内像素，距离场判定覆盖（AGENTS.md 3.7 节）。
//! 头部几何由 [`arrow_head_wings`] 统一提供（三角的两个底角），
//! 导出/egui 预览/命中测试三处同源。

use crate::annotation::Color;

use super::rect::blend_pixel;

/// 头部长度系数（× 线宽；2026-09-10 用户实机：4w 实心头显胖，收至 3w）。
const HEAD_LEN_FACTOR: f32 = 3.0;
/// 两翼与前进方向夹角（±158°，即内侧 22°；2026-09-10 瘦头：25° 太开显坨）。
const WING_ANGLE: f32 = std::f32::consts::PI * 158.0 / 180.0;

/// 箭头两翼端点（从终点 `to` 向起点侧张开）。
///
/// 导出（本模块）、egui 预览（editor.rs）与命中测试共用，保证三处形状一致。
pub fn arrow_head_wings(
    from: (f32, f32),
    to: (f32, f32),
    stroke_width: f32,
) -> ((f32, f32), (f32, f32)) {
    let w = stroke_width.max(1.0);
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 0.5 {
        return (to, to);
    }
    let (dir_x, dir_y) = (dx / len, dy / len);
    let head_len = HEAD_LEN_FACTOR * w;
    let wing = |angle: f32| {
        let (s, c) = angle.sin_cos();
        let rx = dir_x * c - dir_y * s;
        let ry = dir_x * s + dir_y * c;
        (to.0 + rx * head_len, to.1 + ry * head_len)
    };
    (wing(WING_ANGLE), wing(-WING_ANGLE))
}

/// 轴线终点（三角底边中心，`to` 向起点侧退一个头长）。
///
/// 2026-09-10 锐尖根因：轴线若画到 `to`，其平头端帽宽=线宽 w，而三角尖端
/// 附近比轴线细（半宽 d·tan22° 要 d≥1.2w 才盖过 w/2）——最后一段轮廓其实是
/// 轴线的平头端，尖端被截平成宽 w 的平头。轴线只画到底边中心（底半宽
/// 3w·tan22°≈1.21w > w/2，平头端完整埋进实心三角内），尖端才纯粹是三角
/// 的锐角顶点。导出/预览同源换用；命中测试仍测整段（拖动友好）。
pub fn arrow_shaft_end(
    from: (f32, f32),
    to: (f32, f32),
    stroke_width: f32,
) -> (f32, f32) {
    let w = stroke_width.max(1.0);
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 0.5 {
        return to;
    }
    let head_len = HEAD_LEN_FACTOR * w;
    (
        to.0 - dx / len * head_len,
        to.1 - dy / len * head_len,
    )
}

/// 在导出图上绘制箭头（坐标为图像本地像素，越界部分自动裁剪）。
///
/// * `from` - 起点（本地坐标）；
/// * `to` - 终点（箭头尖端，本地坐标）；
/// * `color` - 描边颜色；
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
    let (wing1, wing2) = arrow_head_wings(from, to, w);
    let shaft_end = arrow_shaft_end(from, to, w);

    // 包围盒（ shaft + head 扩大 half ）
    let min_x = from.0.min(to.0).min(wing1.0).min(wing2.0) - half - 1.0;
    let max_x = from.0.max(to.0).max(wing1.0).max(wing2.0) + half + 1.0;
    let min_y = from.1.min(to.1).min(wing1.1).min(wing2.1) - half - 1.0;
    let max_y = from.1.max(to.1).max(wing1.1).max(wing2.1) + half + 1.0;
    let x0 = (min_x.floor() as i32).clamp(0, img.width() as i32);
    let y0 = (min_y.floor() as i32).clamp(0, img.height() as i32);
    let x1 = (max_x.ceil() as i32).clamp(0, img.width() as i32);
    let y1 = (max_y.ceil() as i32).clamp(0, img.height() as i32);

    let tol = half;
    for py in y0..y1 {
        for px in x0..x1 {
            let p = (px as f32 + 0.5, py as f32 + 0.5);
            // 轴线只画到三角底边中心（平头端埋进实心三角内，见 arrow_shaft_end）
            // + 实心三角头（t, wing1, wing2，无描边填充，边缘 0.5px AA）。
            // 尖端 = 三角锐角顶点，干净尖头（轴线不再把尖端截平）。
            let cov = segment_coverage(p, from, shaft_end, tol)
                .max(filled_tri_coverage(p, to, wing1, wing2));
            if cov > 0.0 {
                let mut c = color;
                c.a = (c.a as f32 * cov) as u8;
                if c.a > 0 {
                    blend_pixel(img.get_pixel_mut(px as u32, py as u32), c);
                }
            }
        }
    }
}

/// 实心三角形内的像素覆盖率（三边半平面取交，边缘各 0.5px AA 过渡）。
///
/// 顶点 winding 不固定：先按符号面积统一为逆时针，再取各边左法线距离
/// （逆时针三角内部恒在各边左侧），三边最小值即覆盖率。
fn filled_tri_coverage(p: (f32, f32), a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> f32 {
    let area = (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0);
    let (b, c) = if area < 0.0 { (c, b) } else { (b, c) };
    let edge = |p0: (f32, f32), p1: (f32, f32)| {
        let ex = p1.0 - p0.0;
        let ey = p1.1 - p0.1;
        let len = (ex * ex + ey * ey).sqrt().max(1e-6);
        // 左法线距离（内为正）
        ((p.0 - p0.0) * (-ey) + (p.1 - p0.1) * ex) / len
    };
    (edge(a, b) + 0.5)
        .min(edge(b, c) + 0.5)
        .min(edge(c, a) + 0.5)
        .clamp(0.0, 1.0)
}

/// 平头端帽线段的像素覆盖率（垂直方向与沿段方向各 0.5px 过渡带）。
fn segment_coverage(p: (f32, f32), a: (f32, f32), b: (f32, f32), tol: f32) -> f32 {
    let abx = b.0 - a.0;
    let aby = b.1 - a.1;
    let len2 = abx * abx + aby * aby;
    if len2 < 1e-6 {
        let d = ((p.0 - a.0).powi(2) + (p.1 - a.1).powi(2)).sqrt();
        return (tol + 0.5 - d).clamp(0.0, 1.0);
    }
    let len = len2.sqrt();
    // 未钳制的投影参数（对无限直线取垂直距离，端部裁剪交给 along 因子）
    let t = ((p.0 - a.0) * abx + (p.1 - a.1) * aby) / len2;
    let proj = (a.0 + t * abx, a.1 + t * aby);
    let d = ((p.0 - proj.0).powi(2) + (p.1 - proj.1).powi(2)).sqrt();
    let along = t * len;
    let cov_a = (along + 0.5).clamp(0.0, 1.0);
    let cov_b = (len - along + 0.5).clamp(0.0, 1.0);
    (tol + 0.5 - d).clamp(0.0, 1.0) * cov_a * cov_b
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
        // 箭头尖端附近有三角着色（AA 部分覆盖，不再断言纯色）
        assert_ne!(px(&img, 43, 20), [255, 255, 255, 255]);
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

    #[test]
    fn tip_sharp_no_notch() {
        // 实心尖三角 + 轴线缩短：尖端纯粹是三角锐角顶点（无平头截断、无圆角 blob）
        let mut img = blank();
        draw_arrow(&mut img, (5.0, 20.0), (45.0, 20.0), Color::RED, 6.0);
        // 头部内部（三角内）实心
        assert_eq!(px(&img, 40, 20)[0..3], [255, 59, 48]);
        // 尖端之外干净
        assert_eq!(px(&img, 47, 20), [255, 255, 255, 255]);
        assert_eq!(px(&img, 50, 20), [255, 255, 255, 255]);
        // 锐角判据：接近尖端的列，覆盖宽度必须远小于轴线宽（6px 线宽的平头
        // 端会盖满 ±3.5px；三角在 d=2.5 处半宽仅 ~1px，17/23 行必须干净）
        assert_eq!(px(&img, 43, 17), [255, 255, 255, 255]);
        assert_eq!(px(&img, 43, 23), [255, 255, 255, 255]);
        // 同列三角体内仍有着色（19 行）
        assert_ne!(px(&img, 43, 19), [255, 255, 255, 255]);
        // 中段轴线照常（17 行在轴半宽 3 内）
        assert_ne!(px(&img, 15, 17), [255, 255, 255, 255]);
    }
}
