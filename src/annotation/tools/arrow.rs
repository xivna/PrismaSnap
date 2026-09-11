//! 箭头标注工具。
//!
//! 简约式样（2026-09-10 用户定稿，2026-09-12 三角放大）：线段 + 实心尖三角头
//! （无描边填充，头长 4 倍线宽、内侧半角 20°；3w/22° 版用户实机嫌三角太小）。
//! 此前开放 V 形（预览/导出各一次，SDR 下尤其明显），改实心填充后缺口无从产生，
//! 预览（egui 实心多边形）与导出（CPU 三角形光栅化）天然一致。
//!
//! 光栅策略：遍历箭头包围盒内像素，距离场判定覆盖（AGENTS.md 3.7 节）。
//! 头部几何由 [`arrow_head_wings`] 统一提供（三角的两个底角），
//! 导出/egui 预览/命中测试三处同源。

use crate::annotation::Color;

use super::rect::blend_pixel;

/// 头部长度系数（× 线宽；2026-09-12 用户实机：3w 三角太小，放大至 4w，
/// 半角同步收至 20° 保瘦）。
const HEAD_LEN_FACTOR: f32 = 4.0;
/// 两翼与前进方向夹角（±160°，即内侧 20°；2026-09-10 瘦头：25° 太开显坨；
/// 2026-09-12 头长放大到 4w 后半角再收 2°，不大坨）。
const WING_ANGLE: f32 = std::f32::consts::PI * 160.0 / 180.0;
/// 两翼内侧半角（`PI - WING_ANGLE`），底边轴向位置与重叠量计算用。
const WING_HALF: f32 = std::f32::consts::PI - WING_ANGLE;
/// 三角头长下限（物理像素；2026-09-12 用户实机：最小档线宽的三角太小）。
/// 头长 = `HEAD_LEN_FACTOR × w` 保底此值，两翼/轴线/命中三处同源（经 `head_len`）。
const HEAD_MIN_LEN: f32 = 16.0;
/// 可视轴线最小长度（物理像素；短于此就不画轴线只画头，见 `arrow_shaft_span`）。
const MIN_SHAFT_VISIBLE: f32 = 3.0;

/// 三角头长（线宽比例 + 下限保底）。
fn head_len(stroke_width: f32) -> f32 {
    (HEAD_LEN_FACTOR * stroke_width.max(1.0)).max(HEAD_MIN_LEN)
}

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
    let head_len = head_len(w);
    let wing = |angle: f32| {
        let (s, c) = angle.sin_cos();
        let rx = dir_x * c - dir_y * s;
        let ry = dir_x * s + dir_y * c;
        (to.0 + rx * head_len, to.1 + ry * head_len)
    };
    (wing(WING_ANGLE), wing(-WING_ANGLE))
}

/// 轴线终点（三角内部、底边之前一个重叠量）。
///
/// 两段历史教训（都在实机截图上现形）：
/// ① 2026-09-10 锐尖根因：轴线若画到 `to`，其平头端帽宽 = 线宽 w，而三角尖端
/// 附近比轴线细——最后一段轮廓其实是轴线的平头端，尖端被截平成宽 w 的平头。
/// ② 2026-09-12 接缝根因（用户截图：三角与线段之间一条淡缝）：两翼端点在轴向
/// 上只退 `head_len·cos(半角)`（≈0.94·head_len），而旧代码把轴线退了整整一个
/// `head_len`——轴线平头端落在三角底边之后约 `0.06·head_len`（w=6 时 ~1.5px），
/// 轴线端帽 AA 淡出带与三角底边 AA 淡出带之间留了一条谁都盖不住的淡带.
/// 正确做法：轴线端按底边真实轴向位置再向尖端伸入一个重叠量，平头端埋在三角
/// 实心区内（该处三角半宽 ≈3w·tan20° ≫ w/2 + AA），`max()` 取并集恒为 1，
/// 双重 AA 缝从根上消失。导出/预览同源换用；命中测试仍测整段（拖动友好）。
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
    let head_len = head_len(w);
    // 底边轴向距离（两翼投影）减去重叠量 = 轴线实际长度回退
    let overlap = (w * 0.5).max(1.0);
    let back = (head_len * WING_HALF.cos() - overlap).max(0.0);
    (
        to.0 - dx / len * back,
        to.1 - dy / len * back,
    )
}

/// 轴线绘制区间（`from` → 伸入三角的重叠端；导出/预览共用）。
///
/// 短箭头只画头（2026-09-12 用户实机：箭头很短、或回退拖动时轴线会从
/// 三角前面/尾侧露出来；用户定稿"往前拖出一段距离再画线段"）：
/// 轴线可视长度（`len - back`）不足 `MIN_SHAFT_VISIBLE` 时返回 `None`，
/// 调用方只画三角头。那时根本没有轴线，穿帮在定义上不可能发生。
/// `len < 0.5` 的退化点同样返回 `None`（调用方原有退化逻辑不变）。
pub fn arrow_shaft_span(
    from: (f32, f32),
    to: (f32, f32),
    stroke_width: f32,
) -> Option<((f32, f32), (f32, f32))> {
    let w = stroke_width.max(1.0);
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 0.5 {
        return None;
    }
    let head_len = head_len(w);
    let overlap = (w * 0.5).max(1.0);
    let back = (head_len * WING_HALF.cos() - overlap).max(0.0);
    if len < back + MIN_SHAFT_VISIBLE {
        return None;
    }
    Some((from, arrow_shaft_end(from, to, stroke_width)))
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
    let shaft = arrow_shaft_span(from, to, w);

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
            // 轴线（短箭头无轴线，只画三角头，见 arrow_shaft_span）
            // + 实心三角头（t, wing1, wing2，无描边填充，边缘 0.5px AA）。
            // 尖端 = 三角锐角顶点，干净尖头（轴线不再把尖端截平）；
            // 接缝 = 轴线端帽淡出带与底边淡出带不再首尾相接（重叠区恒为 1）。
            let cov = shaft
                .map(|(a, b)| segment_coverage(p, a, b, tol))
                .unwrap_or(0.0)
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
        // 实心尖三角 + 轴线伸入重叠：尖端纯粹是三角锐角顶点（无平头截断、无圆角 blob）
        let mut img = blank();
        draw_arrow(&mut img, (5.0, 20.0), (45.0, 20.0), Color::RED, 6.0);
        // 头部内部（三角内）实心
        assert_eq!(px(&img, 40, 20)[0..3], [255, 59, 48]);
        // 尖端之外干净
        assert_eq!(px(&img, 47, 20), [255, 255, 255, 255]);
        assert_eq!(px(&img, 50, 20), [255, 255, 255, 255]);
        // 锐角判据：接近尖端的列，覆盖宽度必须远小于轴线宽（6px 线宽的平头
        // 端会盖满 ±3.5px；三角在 d=1.5 处半宽仅 ~0.55px，17/23 行必须干净）
        assert_eq!(px(&img, 43, 17), [255, 255, 255, 255]);
        assert_eq!(px(&img, 43, 23), [255, 255, 255, 255]);
        // 同列三角体内仍有着色（19 行）
        assert_ne!(px(&img, 43, 19), [255, 255, 255, 255]);
        // 中段轴线照常（17 行在轴半宽 3 内）
        assert_ne!(px(&img, 15, 17), [255, 255, 255, 255]);
    }

    #[test]
    fn shaft_triangle_joint_no_gap() {
        // 接缝回归（2026-09-12 用户截图）：旧几何轴线退整整一个头长，平头端落
        // 在三角底边之后 ~1.5px，端帽淡出带与底边淡出带之间留一条淡缝
        //（旧代码下 x=26/27/28 轴心半透或全空）。新几何轴线伸入三角重叠，
        // 自轴尾到尖端整条轴心必须实心。
        let mut img = blank();
        draw_arrow(&mut img, (5.0, 20.0), (45.0, 20.0), Color::RED, 6.0);
        for x in [20, 21, 22, 23, 24, 26, 27, 28, 30] {
            assert_eq!(px(&img, x, 20)[0..3], [255, 59, 48], "轴心 x={x} 应实心");
        }
        // 接缝区上下两行同样实心（三角内半宽远大于 2px）
        for (x, y) in [(22, 18), (22, 22), (26, 17), (26, 23)] {
            assert_eq!(px(&img, x, y)[0..3], [255, 59, 48], "接缝附近 ({x},{y}) 应实心");
        }
    }

    #[test]
    fn min_head_len_floor() {
        // 最小头保底（2026-09-12 用户实机：最小档线宽的三角太小）：
        // w=1 时比例头长仅 4px，保底 16px。两翼约 (30, 14.5)/(30, 25.5)，
        // (30,16)/(30,24) 应在三角内；无保底时这两点全空。
        let mut img = blank();
        draw_arrow(&mut img, (5.0, 20.0), (45.0, 20.0), Color::RED, 1.0);
        assert_eq!(px(&img, 30, 16)[0..3], [255, 59, 48]);
        assert_eq!(px(&img, 30, 24)[0..3], [255, 59, 48]);
        assert_eq!(px(&img, 38, 20)[0..3], [255, 59, 48]);
    }

    #[test]
    fn short_arrow_head_only_no_shaft() {
        // 短箭头只画头（2026-09-12 用户定稿：往前拖出一段才画线段）：
        // w=6 时头长 24、轴线回退约 19.6，可视轴线需 3px；len=5 直接无轴线。
        assert!(arrow_shaft_span((30.0, 20.0), (35.0, 20.0), 6.0).is_none());
        // 正常长度有轴线
        assert!(arrow_shaft_span((5.0, 20.0), (45.0, 20.0), 6.0).is_some());
        // 像素行为：短箭头三角肚子里实心，尖端前方干净
        let mut img = blank();
        draw_arrow(&mut img, (30.0, 20.0), (35.0, 20.0), Color::RED, 6.0);
        assert_eq!(px(&img, 30, 20)[0..3], [255, 59, 48]);
        assert_eq!(px(&img, 28, 20)[0..3], [255, 59, 48]);
        assert_eq!(px(&img, 37, 20), [255, 255, 255, 255]);
        assert_eq!(px(&img, 40, 20), [255, 255, 255, 255]);
    }
}
