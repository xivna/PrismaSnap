//! 背景擦除 + 译文渲染（复用 `annotation/tools/text.rs` 字体链路，见 AGENTS.md 3.8 节）。
//!
//! - 截图多为纯色/UI 背景：采样 bbox 边缘像素众数色矩形填充擦除，
//!   不引入 inpainting 模型（复杂照片背景的 LaMa-onnx 预留同级插件位）；
//! - 字号初值 `est_font_size`（bbox 高反推），译文超宽则按实测宽度比例缩小
//!   至最小可读，仍超则由 [`draw_text_in_rect`](crate::annotation::tools::text::draw_text_in_rect)
//!   自动换行；字体不逐一还原（粗细分加粗由管线组装层定，颜色像素采样）；
//! - 坐标约定同 [`apply_to_image`](crate::annotation::apply_to_image)：
//!   `TranslatedRegion.bbox` 为全图物理像素，`origin` 为选区原点，图内本地坐标系绘制。
//!
//! 本模块为跨平台纯逻辑（`ab_glyph` 系统字体 + `image` 像素操作），可在 WSL2 下单测。

use std::collections::HashMap;

use image::RgbaImage;

use crate::annotation::tools::text::{draw_text_in_rect, measure_line_width};
use crate::annotation::Color;
use crate::ocr::{BBox, TranslatedRegion};
use crate::utils::math::Rect;

/// 最小可读字号（物理像素，与文字工具下限一致）。
pub const MIN_FONT_SIZE: f32 = 8.0;
/// 导出字号上限（物理像素，与文字工具上限一致）。
pub const MAX_FONT_SIZE: f32 = 120.0;
/// 边缘采样步长（物理像素，每 N 像素采一个，提速且众数稳定）。
pub const EDGE_SAMPLE_STEP: u32 = 2;
/// 擦除外扩（物理像素，盖住抗锯齿残边；钳制在图内，不污染相邻行）。
pub const ERASE_PAD: i32 = 1;
/// 判定"文字色区别于背景"的色差下限（RGB 差绝对值之和，满量程 765）。
pub const TEXT_BG_DIFF: i32 = 90;

/// 采样 bbox 边缘像素众数色（背景色估计）。
///
/// 只采上下两行、左右两列（步长 [`EDGE_SAMPLE_STEP`]），截图纯色/UI 背景下
/// 众数即背景；bbox 越界部分自动跳过，全越界时返回白色。
/// 泛型支持 `DynamicImage`（管线采样）与 `RgbaImage`（测试）两种输入。
pub fn sample_bg_color<I>(img: &I, bbox: BBox) -> [u8; 3]
where
    I: image::GenericImageView<Pixel = image::Rgba<u8>>,
{
    let (w, h) = (img.width() as i32, img.height() as i32);
    let x0 = (bbox.x as i32).clamp(0, w.saturating_sub(1));
    let y0 = (bbox.y as i32).clamp(0, h.saturating_sub(1));
    let x1 = (bbox.x as i32 + bbox.width as i32 - 1).clamp(0, w.saturating_sub(1));
    let y1 = (bbox.y as i32 + bbox.height as i32 - 1).clamp(0, h.saturating_sub(1));
    if w <= 0 || h <= 0 || x1 < x0 || y1 < y0 {
        return [255, 255, 255];
    }
    let mut votes: HashMap<[u8; 3], u32> = HashMap::new();
    let mut vote = |x: i32, y: i32| {
        let p = img.get_pixel(x as u32, y as u32).0;
        *votes.entry([p[0], p[1], p[2]]).or_insert(0) += 1;
    };
    let step = EDGE_SAMPLE_STEP.max(1) as i32;
    let mut x = x0;
    while x <= x1 {
        vote(x, y0);
        if y1 != y0 {
            vote(x, y1);
        }
        x += step;
    }
    let mut y = y0 + step;
    while y < y1 {
        vote(x0, y);
        if x1 != x0 {
            vote(x1, y);
        }
        y += step;
    }
    votes
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(c, _)| c)
        .unwrap_or([255, 255, 255])
}

/// 采样 bbox 内部文字颜色（原文字像素采样）。
///
/// 取内部与背景色差 `>` [`TEXT_BG_DIFF`] 的像素众数色；无此类像素
/// （如空框）时按背景明暗回退黑/白，保证译文可读。
/// 泛型支持 `DynamicImage` 与 `RgbaImage` 两种输入（同 [`sample_bg_color`]）。
pub fn sample_text_color<I>(img: &I, bbox: BBox, bg: [u8; 3]) -> [u8; 3]
where
    I: image::GenericImageView<Pixel = image::Rgba<u8>>,
{
    let (w, h) = (img.width() as i32, img.height() as i32);
    let x0 = (bbox.x as i32).clamp(0, w);
    let y0 = (bbox.y as i32).clamp(0, h);
    let x1 = (bbox.x as i32 + bbox.width as i32).clamp(0, w);
    let y1 = (bbox.y as i32 + bbox.height as i32).clamp(0, h);
    let mut votes: HashMap<[u8; 3], u32> = HashMap::new();
    for y in (y0..y1).step_by(EDGE_SAMPLE_STEP.max(1) as usize) {
        for x in (x0..x1).step_by(EDGE_SAMPLE_STEP.max(1) as usize) {
            let p = img.get_pixel(x as u32, y as u32).0;
            let c = [p[0], p[1], p[2]];
            if color_diff(c, bg) > TEXT_BG_DIFF {
                *votes.entry(c).or_insert(0) += 1;
            }
        }
    }
    if let Some((c, _)) = votes.into_iter().max_by_key(|(_, n)| *n) {
        return c;
    }
    // 回退：背景亮则黑字，背景暗则白字
    let luma = 0.299 * bg[0] as f32 + 0.587 * bg[1] as f32 + 0.114 * bg[2] as f32;
    if luma > 128.0 { [0, 0, 0] } else { [255, 255, 255] }
}

/// RGB 差绝对值之和（0~765）。
fn color_diff(a: [u8; 3], b: [u8; 3]) -> i32 {
    (a[0] as i32 - b[0] as i32).abs()
        + (a[1] as i32 - b[1] as i32).abs()
        + (a[2] as i32 - b[2] as i32).abs()
}

/// 背景擦除：众数色矩形填充 bbox（含 [`ERASE_PAD`] 外扩，钳制在图内）。
pub fn erase_background(img: &mut RgbaImage, bbox: BBox, bg: [u8; 3]) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    let x0 = (bbox.x as i32 - ERASE_PAD).clamp(0, w);
    let y0 = (bbox.y as i32 - ERASE_PAD).clamp(0, h);
    let x1 = (bbox.x as i32 + bbox.width as i32 + ERASE_PAD).clamp(0, w);
    let y1 = (bbox.y as i32 + bbox.height as i32 + ERASE_PAD).clamp(0, h);
    let fill = image::Rgba([bg[0], bg[1], bg[2], 255]);
    for y in y0..y1 {
        for x in x0..x1 {
            *img.get_pixel_mut(x as u32, y as u32) = fill;
        }
    }
}

/// 字号自适应：按实测宽度等比缩小，底限 [`MIN_FONT_SIZE`]（仍超则调用方自动换行）。
///
/// - `initial`：初值（原文字 `est_font_size`，钳制到 8~120）；
/// - `translated`：译文（按最宽行度量，`\n` 分行取最大）；
/// - `max_width`：可用宽度（bbox 宽减内边距）；`<=0` 时保守返回初值（不猜）。
/// - `bold`：是否加粗（度量时计入加粗增宽，与绘制一致）。
pub fn fit_font_size(initial: f32, translated: &str, max_width: f32, bold: bool) -> f32 {
    let initial = initial.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
    if max_width <= 0.0 {
        return initial;
    }
    let widest = translated
        .split('\n')
        .map(|line| measure_line_width(line, initial, bold))
        .fold(0.0_f32, f32::max);
    if widest <= 0.0 || widest <= max_width {
        return initial;
    }
    (initial * (max_width / widest)).clamp(MIN_FONT_SIZE, initial)
}

/// 全图 bbox 按选区原点平移为图内本地矩形（与 `apply_to_image` 同约定）。
pub fn shift_bbox(bbox: BBox, origin: (i32, i32)) -> Rect {
    Rect {
        x: bbox.x as i32 - origin.0,
        y: bbox.y as i32 - origin.1,
        width: bbox.width,
        height: bbox.height,
    }
}

/// 渲染单个译文区域到图上（背景擦除 + 自适应字号 + 文字工具绘制）。
///
/// - `region.bg_color` / `text_color` 由管线组装层事先采样填好，本函数只消费；
/// - `bold` 由组装层按笔画粗细定（当前恒 false，笔画分析后续补）；
/// - 越界 bbox 自动钳制，不 panic。
pub fn render_translated_region(
    img: &mut RgbaImage,
    region: &TranslatedRegion,
    bold: bool,
    origin: (i32, i32),
) {
    // 本地坐标（全图 bbox - 选区原点）
    let local = BBox {
        x: (region.bbox.x as i32 - origin.0).max(0) as u32,
        y: (region.bbox.y as i32 - origin.1).max(0) as u32,
        width: region.bbox.width,
        height: region.bbox.height,
    };
    erase_background(img, local, region.bg_color);
    let rect = shift_bbox(region.bbox, origin);
    let avail_w = rect.width as f32 - 4.0; // 与 draw_text_in_rect 内边距对齐
    let size = fit_font_size(region.est_font_size as f32, &region.translated, avail_w, bold);
    let color = Color {
        r: region.text_color[0],
        g: region.text_color[1],
        b: region.text_color[2],
        a: 255,
    };
    draw_text_in_rect(img, rect, &region.translated, color, size, bold);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 白底黑字合成图（bbox 内白底 + 中部黑条模拟文字行）。
    fn fixture() -> RgbaImage {
        let mut img = RgbaImage::from_pixel(100, 40, image::Rgba([255, 255, 255, 255]));
        for y in 14..26 {
            for x in 12..88 {
                *img.get_pixel_mut(x, y) = image::Rgba([10, 10, 10, 255]);
            }
        }
        img
    }

    #[test]
    fn bg_samples_white_text_samples_black() {
        let img = fixture();
        let bbox = BBox { x: 10, y: 10, width: 80, height: 20 };
        assert_eq!(sample_bg_color(&img, bbox), [255, 255, 255]);
        assert_eq!(sample_text_color(&img, bbox, [255, 255, 255]), [10, 10, 10]);
    }

    #[test]
    fn empty_box_falls_back_to_readable() {
        let img = RgbaImage::from_pixel(20, 20, image::Rgba([255, 255, 255, 255]));
        let bbox = BBox { x: 5, y: 5, width: 10, height: 10 };
        // 全白框：无文字色，回退黑字
        assert_eq!(sample_text_color(&img, bbox, [255, 255, 255]), [0, 0, 0]);
        // 全黑框：回退白字
        let dark = RgbaImage::from_pixel(20, 20, image::Rgba([5, 5, 5, 255]));
        assert_eq!(sample_text_color(&dark, bbox, [5, 5, 5]), [255, 255, 255]);
    }

    #[test]
    fn erase_fills_and_clamps() {
        let mut img = fixture();
        erase_background(&mut img, BBox { x: 10, y: 10, width: 80, height: 20 }, [255, 255, 255]);
        // 黑条被盖掉
        assert_eq!(img.get_pixel(50, 20).0, [255, 255, 255, 255]);
        // 越界擦除不 panic
        erase_background(&mut img, BBox { x: 90, y: 30, width: 50, height: 30 }, [0, 0, 0]);
        assert_eq!(img.get_pixel(99, 39).0, [0, 0, 0, 255]);
    }

    #[test]
    fn fit_keeps_short_shrinks_long() {
        // 短译文保持初值
        let keep = fit_font_size(20.0, "Hi", 200.0, false);
        assert!((keep - 20.0).abs() < f32::EPSILON);
        // 长译文等比缩小但不低于下限
        let small = fit_font_size(20.0, "这是一段很长很长很长很长很长的译文内容", 60.0, false);
        assert!(small < 20.0 && small >= MIN_FONT_SIZE);
        // 空译文/零宽度保守处理
        assert_eq!(fit_font_size(20.0, "", 60.0, false), 20.0);
        assert_eq!(fit_font_size(20.0, "Hi", 0.0, false), 20.0);
    }

    #[test]
    fn shift_matches_apply_to_image_convention() {
        let r = shift_bbox(BBox { x: 20, y: 30, width: 15, height: 10 }, (10, 20));
        assert_eq!((r.x, r.y, r.width, r.height), (10, 10, 15, 10));
    }

    #[test]
    fn render_end_to_end_draws_text() {
        let mut img = RgbaImage::from_pixel(120, 40, image::Rgba([255, 255, 255, 255]));
        let region = TranslatedRegion {
            bbox: BBox { x: 10, y: 8, width: 100, height: 24 },
            original: String::from("Hello"),
            translated: String::from("ABC"),
            est_font_size: 20,
            bg_color: [255, 255, 255],
            text_color: [0, 0, 0],
        };
        render_translated_region(&mut img, &region, false, (0, 0));
        // 框内应出现非白像素（译文笔画）
        let mut ink = 0;
        for y in 8..32 {
            for x in 10..110 {
                if img.get_pixel(x, y).0[0] < 128 {
                    ink += 1;
                }
            }
        }
        assert!(ink > 10, "译文应有可见笔画，ink={ink}");
    }
}
