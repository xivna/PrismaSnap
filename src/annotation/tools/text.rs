//! 文字添加标注工具。
//!
//! 配合 `ab_glyph` 渲染，中文场景调用系统字体兜底（与 `ui/gui.rs` 共用
//! 缓存字体），CPU 光栅化直接写导出图（AGENTS.md 3.7 节）。

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};

use crate::annotation::{CharStyle, Color};
use super::rect::blend_pixel;

/// 在矩形文本框内绘制文字（带自动换行，超出宽度按字符 wrapping）。
///
/// `rect` 为物理像素文本框（`x,y` 为左上，`width` 为可用宽度，高度超出可溢出）。
pub fn draw_text_in_rect(
    img: &mut image::RgbaImage,
    rect: crate::utils::math::Rect,
    content: &str,
    color: Color,
    font_size: f32,
    bold: bool,
    font: Option<&str>,
) {
    if content.trim().is_empty() || color.a == 0 || rect.width < 4 || rect.height < 4 {
        // 空内容或过小矩形不绘制（避免除零）
        if content.trim().is_empty() { return; }
    }
    let font_size = font_size.clamp(8.0, 120.0);
    let (font_bytes, simulate_bold) = if bold {
        if let Some(b) = cjk_font_bytes_bold(font) { (b, false) } else if let Some(n) = cjk_font_bytes(font) { (n, true) } else { tracing::warn!("未找到字体"); return; }
    } else if let Some(n) = cjk_font_bytes(font) { (n, false) } else { tracing::warn!("未找到字体"); return; };
    let font = match FontRef::try_from_slice(&font_bytes) { Ok(f) => f, Err(e) => { tracing::warn!("字体解析失败: {e:?}"); return; } };
    let scale = PxScale::from(font_size);
    let scaled = font.as_scaled(scale);
    let line_height = font_size * 1.25;
    let max_w = rect.width as f32 - 4.0; // 左右各 2px 内边距
    // 首行基线 = 框顶 + 内边距(2) + 字号
    let first_baseline = rect.y as f32 + 2.0 + font_size * 0.85;
    let mut line_idx: usize = 0;
    for orig_line in content.split('\n') {
        // 对每行做宽度 wrapping（按字符累加 h_advance）
        let wrapped = if max_w > 20.0 { wrap_line(orig_line, &font, scale, max_w) } else { vec![orig_line.to_string()] };
        // 空行仍占一行高度
        if wrapped.is_empty() {
            line_idx += 1;
            continue;
        }
        for wl in wrapped {
            let baseline_y = first_baseline + line_idx as f32 * line_height;
            // 超出矩形底部的裁剪：若整行已超出图片或矩形较多，可跳过绘制（仍占行高）
            // 允许溢出框底 1 行（与 egui TextEdit 行为一致，框会随内容自增高，但导出固定框高仍尽量画）
            let mut caret_x = rect.x as f32 + 2.0;
            if wl.is_empty() {
                line_idx += 1;
                continue;
            }
            for ch in wl.chars() {
                let gid = font.glyph_id(ch);
                let glyph = gid.with_scale_and_position(scale, ab_glyph::point(caret_x, baseline_y));
                if let Some(outlined) = font.outline_glyph(glyph) {
                    let bounds = outlined.px_bounds();
                    let offsets: &[(f32,f32)] = if simulate_bold { &[(0.0,0.0),(0.9,0.0),(0.0,0.9),(0.9,0.9)] } else { &[(0.0,0.0)] };
                    for (ox, oy) in offsets {
                        outlined.draw(|x, y, cov| {
                            // 注意偏移要先加再取整（原写法 `min as i32 + x + ox as i32`
                            // 各自截断，0.9px 偏移恒为 0——模拟加粗一直是 no-op，
                            // Windows 有 msyhbd 真粗体从未暴露，2026-09-10 单测抓出）
                            let px = ((bounds.min.x + *ox).round() as i32 + x as i32) as u32;
                            let py = ((bounds.min.y + *oy).round() as i32 + y as i32) as u32;
                            if px >= img.width() || py >= img.height() { return; }
                            let a = (color.a as f32 * cov) as u8; if a==0 {return;}
                            let col = Color{r:color.r,g:color.g,b:color.b,a};
                            blend_pixel(img.get_pixel_mut(px,py), col);
                        });
                    }
                    caret_x += scaled.h_advance(gid);
                } else {
                    caret_x += scaled.h_advance(gid);
                }
            }
            line_idx += 1;
        }
        // orig_line 为 "" 时上述 wrapped 为 [""] 已处理空行占位，不再额外加
    }
}

/// 富文本版 [`draw_text_in_rect`]：逐字符颜色/加粗/字体，**字号整框统一**
/// （2026-09-10 用户定稿），换行/行高/基线与统一版一致，保证混合与统一
/// 渲染的排版相同。
///
/// * `char_styles` - 逐字符覆盖（空或长度不符时整框回退基础样式）；
/// * `font_table` - 框内字体路径表（`CharStyle.font` 下标）。
pub fn draw_rich_text_in_rect(
    img: &mut image::RgbaImage,
    rect: crate::utils::math::Rect,
    content: &str,
    base_color: Color,
    base_bold: bool,
    base_font: Option<&str>,
    font_size: f32,
    char_styles: &[CharStyle],
    font_table: &[String],
) {
    if content.trim().is_empty() || rect.width < 4 || rect.height < 4 {
        return;
    }
    let font_size = font_size.clamp(8.0, 120.0);
    let base = CharStyle::base(base_color, base_bold);
    // 逐字符覆盖仅在长度与字符数一致时生效（防外部传入坏数据部分套用）
    let styles_valid = char_styles.len() == content.chars().count();
    let style_of = |i: usize| -> CharStyle {
        if styles_valid { char_styles.get(i).copied().unwrap_or(base) } else { base }
    };
    // 字体槽：样式签名（字体路径, 加粗）→ (字节, 模拟加粗)。同槽复用，加载一次。
    let mut slots: Vec<(Option<String>, bool, Option<std::sync::Arc<Vec<u8>>>, bool)> = Vec::new();
    let resolve_font = |path: Option<&str>, bold: bool| -> (Option<std::sync::Arc<Vec<u8>>>, bool) {
        if bold {
            if let Some(b) = cjk_font_bytes_bold(path) { (Some(b), false) }
            else if let Some(n) = cjk_font_bytes(path) { (Some(n), true) }
            else { (None, true) }
        } else if let Some(n) = cjk_font_bytes(path) { (Some(n), false) } else { (None, false) }
    };
    let slot_of = |slots: &mut Vec<(Option<String>, bool, Option<std::sync::Arc<Vec<u8>>>, bool)>,
                   path: Option<&str>, bold: bool| -> usize {
        if let Some(i) = slots.iter().position(|(p, b, _, _)| *b == bold && p.as_deref() == path) {
            return i;
        }
        let (bytes, sim) = resolve_font(path, bold);
        slots.push((path.map(str::to_string), bold, bytes, sim));
        slots.len() - 1
    };
    // 逐字符计划（char 域；i 对齐 char_styles）
    let mut plan: Vec<(char, usize, Color)> = Vec::new();
    for (i, ch) in content.chars().enumerate() {
        let style = style_of(i);
        let path = style
            .font
            .and_then(|fi| font_table.get(fi as usize).map(String::as_str))
            .or(base_font);
        let slot = slot_of(&mut slots, path, style.bold);
        plan.push((ch, slot, style.color));
    }
    let scale = PxScale::from(font_size);
    // 字体引用一次解析（借用 slots，slots 此后只读）
    let font_refs: Vec<Option<FontRef>> = slots
        .iter()
        .map(|(_, _, bytes, _)| {
            bytes.as_ref().and_then(|b| FontRef::try_from_slice(b).ok())
        })
        .collect();
    let advance_of = |ch: char, slot: usize| -> f32 {
        match font_refs.get(slot).and_then(|f| f.as_ref()) {
            Some(font) => font.as_scaled(scale).h_advance(font.glyph_id(ch)),
            None => font_size * 0.6, // 无字体回退估算（与统一版链路一致）
        }
    };
    let line_height = font_size * 1.25;
    let max_w = rect.width as f32 - 4.0;
    let first_baseline = rect.y as f32 + 2.0 + font_size * 0.85;
    // 画一行（行内字符可各用字体/颜色/加粗；换行决策在外层）
    let mut line_idx: usize = 0;
    let mut pending: Vec<(char, usize, Color)> = Vec::new();
    let mut pend_w = 0.0f32;
    let mut draw_line = |line: &[(char, usize, Color)], line_idx: usize| {
        if line.is_empty() {
            return;
        }
        let baseline_y = first_baseline + line_idx as f32 * line_height;
        let mut caret_x = rect.x as f32 + 2.0;
        for &(ch, slot, color) in line.iter() {
            if color.a == 0 {
                caret_x += advance_of(ch, slot);
                continue;
            }
            if let Some(font) = font_refs.get(slot).and_then(|f| f.as_ref()) {
                let scaled = font.as_scaled(scale);
                let gid = font.glyph_id(ch);
                let glyph = gid.with_scale_and_position(scale, ab_glyph::point(caret_x, baseline_y));
                if let Some(outlined) = font.outline_glyph(glyph) {
                    let bounds = outlined.px_bounds();
                    let sim = slots[slot].3;
                    let offsets: &[(f32, f32)] = if sim {
                        &[(0.0, 0.0), (0.9, 0.0), (0.0, 0.9), (0.9, 0.9)]
                    } else {
                        &[(0.0, 0.0)]
                    };
                    for (ox, oy) in offsets {
                        outlined.draw(|x, y, cov| {
                            // 同上：偏移先加再取整（老写法 0.9px 恒被截断成 0）
                            let px = ((bounds.min.x + *ox).round() as i32 + x as i32) as u32;
                            let py = ((bounds.min.y + *oy).round() as i32 + y as i32) as u32;
                            if px >= img.width() || py >= img.height() { return; }
                            let a = (color.a as f32 * cov) as u8;
                            if a == 0 { return; }
                            blend_pixel(img.get_pixel_mut(px, py), Color { r: color.r, g: color.g, b: color.b, a });
                        });
                    }
                    caret_x += scaled.h_advance(gid);
                } else {
                    caret_x += scaled.h_advance(gid);
                }
            } else {
                caret_x += advance_of(ch, slot);
            }
        }
    };
    // 逐字符 wrapping（宽度用各自字体 advance；行内不同字体共存），按 \n 分段
    for &(ch, slot, color) in plan.iter() {
        if ch == '\n' {
            draw_line(&pending, line_idx);
            line_idx += 1;
            pending.clear();
            pend_w = 0.0;
            continue;
        }
        let w = advance_of(ch, slot);
        if pend_w + w > max_w && !pending.is_empty() {
            draw_line(&pending, line_idx);
            line_idx += 1;
            pending.clear();
            pend_w = 0.0;
        }
        pending.push((ch, slot, color));
        pend_w += w;
    }
    if !pending.is_empty() {
        draw_line(&pending, line_idx);
    }
}
///
/// 与 [`draw_text_in_rect`] 用同一字体选择（粗体优先 `msyhbd`，缺失则模拟加粗约 +0.9px）；
/// 找不到任何系统字体时回退按"0.6 倍字号每字"估算（WSL2 等无 CJK 环境仍可跑通逻辑）。
pub fn measure_line_width(line: &str, font_size: f32, bold: bool, font: Option<&str>) -> f32 {
    let font_size = font_size.clamp(8.0, 120.0);
    if line.is_empty() {
        return 0.0;
    }
    let (font_bytes, simulate_bold) = if bold {
        if let Some(b) = cjk_font_bytes_bold(font) { (b, false) } else if let Some(n) = cjk_font_bytes(font) { (n, true) } else { return line.chars().count() as f32 * font_size * 0.6; }
    } else if let Some(n) = cjk_font_bytes(font) { (n, false) } else { return line.chars().count() as f32 * font_size * 0.6; };
    let font = match FontRef::try_from_slice(&font_bytes) { Ok(f) => f, Err(_) => return line.chars().count() as f32 * font_size * 0.6 };
    let scale = PxScale::from(font_size);
    let scaled = font.as_scaled(scale);
    let w: f32 = line.chars().map(|ch| scaled.h_advance(font.glyph_id(ch))).sum();
    if simulate_bold { w + 0.9 } else { w }
}

/// 将一段文本按 `max_width` 自动换行（物理像素，与 [`draw_text_in_rect`] 同字体链路）。
///
/// 按 `\n` 分段、每段独立 wrapping 后拼回；空段保留为空行。找不到系统字体时
/// 按"0.6 倍字号每字"估算断行（WSL2 等无 CJK 环境仍可跑通逻辑）。
/// 预览层（egui）与导出层（CPU）共用，保证所见即所得。
pub fn wrap_text_for_width(
    content: &str,
    font_size: f32,
    max_width: f32,
    bold: bool,
    font: Option<&str>,
) -> Vec<String> {
    let font_size = font_size.clamp(8.0, 120.0);
    if max_width <= 0.0 {
        return content.split('\n').map(str::to_string).collect();
    }
    let (font_bytes, _) = if bold {
        if let Some(b) = cjk_font_bytes_bold(font) {
            (Some(b), false)
        } else if let Some(n) = cjk_font_bytes(font) {
            (Some(n), true)
        } else {
            (None, true)
        }
    } else if let Some(n) = cjk_font_bytes(font) {
        (Some(n), false)
    } else {
        (None, false)
    };
    let Some(bytes) = font_bytes else {
        // 无字体回退：按字符数估算断行
        let per_char = (font_size * 0.6).max(1.0);
        let per_line = ((max_width / per_char).floor() as usize).max(1);
        let mut out = Vec::new();
        for para in content.split('\n') {
            if para.is_empty() {
                out.push(String::new());
                continue;
            }
            let chars: Vec<char> = para.chars().collect();
            for chunk in chars.chunks(per_line) {
                out.push(chunk.iter().collect());
            }
        }
        return out;
    };
    let font = match FontRef::try_from_slice(&bytes) {
        Ok(f) => f,
        Err(_) => {
            return content.split('\n').map(str::to_string).collect();
        }
    };
    let scale = PxScale::from(font_size);
    let mut out = Vec::new();
    for para in content.split('\n') {
        out.extend(wrap_line(para, &font, scale, max_width));
    }
    out
}

/// 将一行按 max_width 按字符自动换行（返回多行，含原空行）。
fn wrap_line(line: &str, font: &FontRef, scale: PxScale, max_w: f32) -> Vec<String> {
    if line.is_empty() { return vec![String::new()]; }
    let scaled = font.as_scaled(scale);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w: f32 = 0.0;
    for ch in line.chars() {
        let gid = font.glyph_id(ch);
        let w = scaled.h_advance(gid);
        if cur_w + w > max_w && !cur.is_empty() {
            lines.push(cur);
            cur = String::new();
            cur_w = 0.0;
        }
        cur.push(ch);
        cur_w += w;
    }
    if !cur.is_empty() || lines.is_empty() { lines.push(cur); }
    lines
}

/// 普通 CJK 字体字节（标注/翻译字体；进程内缓存，预览与导出共享同一份）。
///
/// 优先用户在设置页选择的字体（`ui.annotation_font`），空则回落系统默认链路
/// （微软雅黑 → 黑体 → 宋体 → Linux DejaVu，WSL2 单测可跑）。
fn cjk_font_bytes(font: Option<&str>) -> Option<std::sync::Arc<Vec<u8>>> {
    if let Some(p) = font {
        return crate::utils::fontsel::load_font_bytes(p);
    }
    if let Some(p) = crate::utils::fontsel::annotation_font_path() {
        return crate::utils::fontsel::load_font_bytes(&p);
    }
    const CANDIDATES: [&str; 4] = [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\simsun.ttc",
    ];
    for path in CANDIDATES {
        if let Some(b) = crate::utils::fontsel::load_font_bytes(path) {
            return Some(b);
        }
    }
    for path in [r"/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"] {
        if let Some(b) = crate::utils::fontsel::load_font_bytes(path) {
            return Some(b);
        }
    }
    None
}

/// 粗体 CJK 字体字节（优先用户字体的粗体变体；缺失返回 None 由上层模拟加粗）。
fn cjk_font_bytes_bold(font: Option<&str>) -> Option<std::sync::Arc<Vec<u8>>> {
    let effective = font.map(str::to_string)
        .or_else(crate::utils::fontsel::annotation_font_path);
    if let Some(p) = effective {
        if let Some(bold_path) = crate::utils::fontsel::bold_variant_path(&p) {
            return crate::utils::fontsel::load_font_bytes(&bold_path);
        }
        // 字体无粗体变体：直接 None 走四向模拟加粗
        return None;
    }
    const CANDIDATES: [&str; 3] = [
        r"C:\Windows\Fonts\msyhbd.ttc",
        r"C:\Windows\Fonts\msyhbd.ttf",
        r"C:\Windows\Fonts\msyh_bold.ttf",
    ];
    for path in CANDIDATES {
        if let Some(b) = crate::utils::fontsel::load_font_bytes(path) {
            return Some(b);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::Color;
    use crate::utils::math::Rect;

    #[test]
    fn rich_mixed_colors() {
        let mut img = image::RgbaImage::from_pixel(80, 30, image::Rgba([255, 255, 255, 255]));
        let styles = vec![
            CharStyle::base(Color::RED, false),
            CharStyle::base(Color::BLUE, false),
        ];
        draw_rich_text_in_rect(
            &mut img,
            Rect { x: 2, y: 2, width: 70, height: 26 },
            "AB",
            Color::BLACK, false, None,
            16.0,
            &styles,
            &[],
        );
        let reds = img.pixels().filter(|p| p[0] > 150 && p[2] < 100).count();
        let blues = img.pixels().filter(|p| p[2] > 150 && p[0] < 100).count();
        assert!(reds > 0, "A 应有红色像素");
        assert!(blues > 0, "B 应有蓝色像素");
    }

    #[test]
    fn rich_sim_bold_covers_more() {
        let count = |bold: bool| -> usize {
            let mut img = image::RgbaImage::from_pixel(60, 30, image::Rgba([255, 255, 255, 255]));
            let styles = vec![CharStyle::base(Color::BLACK, bold)];
            draw_rich_text_in_rect(
                &mut img,
                Rect { x: 2, y: 2, width: 50, height: 26 },
                "H",
                Color::BLACK, bold, None,
                16.0,
                &styles,
                &[],
            );
            img.pixels().filter(|p| p[0] < 250).count()
        };
        // 无粗体变体环境（WSL）走四向模拟加粗，覆盖应严格更多
        assert!(count(true) > count(false));
    }

    #[test]
    fn rich_style_len_mismatch_falls_back_to_base() {
        let mut img = image::RgbaImage::from_pixel(80, 30, image::Rgba([255, 255, 255, 255]));
        // 长度不符（1 vs 3 字符）→ 全部回退基础红色，蓝色不应出现
        let styles = vec![CharStyle::base(Color::BLUE, false)];
        draw_rich_text_in_rect(
            &mut img,
            Rect { x: 2, y: 2, width: 70, height: 26 },
            "ABC",
            Color::RED, false, None,
            16.0,
            &styles,
            &[],
        );
        let reds = img.pixels().filter(|p| p[0] > 150 && p[2] < 100).count();
        let blues = img.pixels().filter(|p| p[2] > 150 && p[0] < 100).count();
        assert!(reds > 0);
        assert_eq!(blues, 0);
    }

    #[test]
    fn rich_newline_and_wrap() {
        // 换行 + 窄框 wrapping 不 panic，且两行都有着色
        let mut img = image::RgbaImage::from_pixel(40, 60, image::Rgba([255, 255, 255, 255]));
        let styles: Vec<CharStyle> = "AA\nBB".chars().map(|_| CharStyle::base(Color::BLACK, false)).collect();
        draw_rich_text_in_rect(
            &mut img,
            Rect { x: 2, y: 2, width: 12, height: 56 },
            "AA\nBB",
            Color::BLACK, false, None,
            12.0,
            &styles,
            &[],
        );
        let row_has = |y: u32| (0..img.width()).any(|x| img.get_pixel(x, y)[0] < 250);
        assert!(row_has(8), "第一行应有文字");
        assert!(row_has(26), "第二行应有文字");
    }

    #[test]
    fn empty_content_draws_nothing() {
        let mut img = image::RgbaImage::from_pixel(20, 20, image::Rgba([255, 255, 255, 255]));
        draw_text_in_rect(&mut img, Rect { x: 2, y: 2, width: 60, height: 40 }, "   ", Color::BLACK, 16.0, false, None);
        assert_eq!(img.get_pixel(10, 10), &image::Rgba([255, 255, 255, 255]));
    }

    #[test]
    fn draw_does_not_panic() {
        let mut img = image::RgbaImage::from_pixel(60, 60, image::Rgba([255, 255, 255, 255]));
        draw_text_in_rect(&mut img, Rect { x: 2, y: 2, width: 56, height: 56 }, "Hi", Color::BLACK, 16.0, false, None);
        draw_text_in_rect(&mut img, Rect { x: 2, y: 2, width: 56, height: 56 }, "Hi\n世界", Color::RED, 18.0, true, None);
    }

    #[test]
    fn empty_multiline_is_degenerate_handled() {
        let mut img = image::RgbaImage::from_pixel(20, 20, image::Rgba([255, 255, 255, 255]));
        draw_text_in_rect(&mut img, Rect { x: 2, y: 2, width: 16, height: 16 }, "\n\n   \n", Color::BLACK, 16.0, false, None);
        assert_eq!(img.get_pixel(10, 10), &image::Rgba([255, 255, 255, 255]));
    }

    #[test]
    fn measure_width_scales_with_font_size() {
        let w16 = measure_line_width("Hello世界", 16.0, false, None);
        let w32 = measure_line_width("Hello世界", 32.0, false, None);
        assert!(w16 > 0.0);
        // 同一字体链路下 advance 随字号线性缩放（模拟加粗 +0.9px，留容差）
        assert!((w32 / w16 - 2.0).abs() < 0.15, "w16={w16} w32={w32}");
        assert_eq!(measure_line_width("", 16.0, false, None), 0.0);
    }

    #[test]
    fn wrap_short_line_is_unchanged() {
        assert_eq!(wrap_text_for_width("你好世界", 20.0, 400.0, false, None), vec!["你好世界"]);
        // 空段保留为空行
        assert_eq!(wrap_text_for_width("甲\n\n乙", 20.0, 400.0, false, None), vec!["甲", "", "乙"]);
    }

    #[test]
    fn wrap_long_line_breaks_and_roundtrips() {
        // WSL2 回退字体（DejaVu）缺 CJK 字形，断行用 ASCII 长行验证逻辑；
        // CJK 路径与导出 `draw_text_in_rect` 共用 `wrap_line`，Windows 实机覆盖。
        let long = "Top 10% salaries in Singapore by age and industry from 25 to 45";
        let lines = wrap_text_for_width(long, 20.0, 100.0, false, None);
        assert!(lines.len() > 1, "应断成多行：{lines:?}");
        // 断行不丢字
        assert_eq!(lines.concat(), long);
        // 每行实测宽度都不超限（含浮点容差）
        for l in &lines {
            assert!(
                measure_line_width(l, 20.0, false, None) <= 100.0 + 1.0,
                "行超宽：{l}"
            );
        }
    }

    #[test]
    fn wrap_nonpositive_width_keeps_paragraphs() {
        assert_eq!(wrap_text_for_width("甲\n乙", 20.0, 0.0, false, None), vec!["甲", "乙"]);
    }
}
