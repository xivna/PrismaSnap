//! 箭头标注工具。
//!
//! 简约线条式样（对齐 Flameshot/ShareX 等业界截图工具，2026-09-09 用户定稿）：
//! 线段直通尖端 + 开放 V 形两翼（无三角形填充，避免填充/描边抗锯齿接缝），
//! 头长 4 倍线宽（原 12 倍过大且盖住线段，拖动稍短就只剩三角）。
//!
//! 光栅策略：遍历箭头包围盒内像素，距离场判定描边覆盖（AGENTS.md 3.7 节）。
//! 头部几何由 [`arrow_head_wings`] 统一提供，导出/egui 预览/命中测试三处同源。

use crate::annotation::Color;

use super::rect::blend_pixel;

/// 头部长度系数（× 线宽）。
const HEAD_LEN_FACTOR: f32 = 4.0;
/// 两翼与前进方向夹角（±155°，即内侧 25°，比旧 30° 略尖更精神）。
const WING_ANGLE: f32 = std::f32::consts::PI * 155.0 / 180.0;

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
            // 三段（轴线 + 两翼）取最大覆盖率。平头端帽 + 双向 AA 羽化：
            // egui 开路径端点是平头（tessellator 仅外扩羽化，无圆帽），导出须对齐
            // （旧版圆头胶囊导致预览/保存端点样式不一致，2026-09-10 实机反馈）
            // 尖端圆角连接盘（对齐 egui 默认 Round join）：三段平头在顶点各自衰减
            // 会让联合覆盖率掉到 ~0.5 出现缺口/毛刺，顶点半径=线宽一半的圆盘补满。
            // 尾部起点仍是平头端帽（与预览一致）。
            let dt = ((p.0 - to.0).powi(2) + (p.1 - to.1).powi(2)).sqrt();
            let cov = segment_coverage(p, from, to, tol)
                .max(segment_coverage(p, to, wing1, tol))
                .max(segment_coverage(p, to, wing2, tol))
                .max((tol + 0.5 - dt).clamp(0.0, 1.0));
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
