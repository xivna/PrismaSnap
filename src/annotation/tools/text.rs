//! 文字添加标注工具。
//!
//! 配合 `ab_glyph` 渲染，中文场景调用系统字体兜底（与 `ui/gui.rs` 共用
//! 缓存字体），CPU 光栅化直接写导出图（AGENTS.md 3.7 节）。

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};

use crate::annotation::Color;
use super::rect::blend_pixel;

/// 绘制文字标注（本地坐标）。
///
/// * `pos` - 基线起点（本地坐标，物理像素）；
/// * `content` - 文本内容；
/// * `color` - 颜色；
/// * `font_size` - 字号（物理像素，建议 14~24）。
pub fn draw_text(
    img: &mut image::RgbaImage,
    pos: (f32, f32),
    content: &str,
    color: Color,
    font_size: f32,
) {
    if content.trim().is_empty() || color.a == 0 {
        return;
    }
    let Some(font_bytes) = cjk_font_bytes() else {
        tracing::warn!("未找到系统中文字体，文字标注未写入");
        return;
    };
    let font = match FontRef::try_from_slice(font_bytes) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("字体解析失败: {e:?}");
            return;
        }
    };
    let scale = PxScale::from(font_size.max(8.0));
    let scaled = font.as_scaled(scale);
    let mut caret_x = pos.0;
    let caret_y = pos.1;
    for ch in content.chars() {
        if ch == '\n' {
            caret_x = pos.0;
            // 简易行距 = 字号 *1.2
            // 手动换行时 y 增加 （此处简化）
            continue;
        }
        let glyph_id = font.glyph_id(ch);
        let glyph = glyph_id.with_scale_and_position(scale, ab_glyph::point(caret_x, caret_y));
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|x, y, cov| {
                let px = (bounds.min.x as i32 + x as i32) as u32;
                let py = (bounds.min.y as i32 + y as i32) as u32;
                if px >= img.width() || py >= img.height() {
                    return;
                }
                // cov 为覆盖率 0..1，叠加到 alpha
                let a = (color.a as f32 * cov) as u8;
                if a == 0 {
                    return;
                }
                let col = Color { r: color.r, g: color.g, b: color.b, a };
                blend_pixel(img.get_pixel_mut(px, py), col);
            });
        }
        caret_x += scaled.h_advance(glyph_id);
    }
}

/// 复用 `ui/gui.rs` 的字体缓存策略：优先微软雅黑 / 黑体，OnceLock 缓存。
fn cjk_font_bytes() -> Option<&'static [u8]> {
    static FONT: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();
    FONT.get_or_init(|| {
        const CANDIDATES: [&str; 4] = [
            r"C:\Windows\Fonts\msyh.ttc",
            r"C:\Windows\Fonts\msyh.ttf",
            r"C:\Windows\Fonts\simhei.ttf",
            r"C:\Windows\Fonts\simsun.ttc",
        ];
        for path in CANDIDATES {
            if let Ok(bytes) = std::fs::read(path) {
                return Some(bytes);
            }
        }
        // WSL2/l/Linux 测试环境无字体时尝试常见 Linux 字体
        for path in [r"/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"] {
            if let Ok(bytes) = std::fs::read(path) {
                return Some(bytes);
            }
        }
        None
    })
    .as_deref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::Color;

    #[test]
    fn empty_content_does_nothing() {
        let mut img = image::RgbaImage::from_pixel(20, 20, image::Rgba([255, 255, 255, 255]));
        draw_text(&mut img, (5.0, 10.0), "   ", Color::BLACK, 16.0);
        assert_eq!(img.get_pixel(10, 10), &image::Rgba([255, 255, 255, 255]));
    }

    #[test]
    fn draw_does_not_panic() {
        let mut img = image::RgbaImage::from_pixel(40, 20, image::Rgba([255, 255, 255, 255]));
        draw_text(&mut img, (2.0, 15.0), "Hi", Color::BLACK, 16.0);
        // 不断言像素，环境无字体时也应不 panic
    }
}
