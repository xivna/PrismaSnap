//! 字体选择弹层（设置页与覆盖层工具条共用，2026-09-09 用户需求）。
//!
//! Word 式体验：列表每一项用**该项字体本身**渲染预览（ab_glyph 离屏光栅化为
//! 纹理，避免把全系统字体注册进 egui——数百字体数 GB 内存不可行）；悬停高亮，
//! 点击提交。悬停回调供工具条"选中文字实时换字体"（Word 同款交互）。

use std::collections::HashMap;

use crate::utils::fontsel::{self, FontChoice};

/// 选择器状态（调用方持有；纹理缓存与 egui ctx 绑定，ctx 销毁即失效重建）。
#[derive(Default)]
pub struct FontPickerState {
    /// 弹层是否展开。
    pub open: bool,
    /// 字体预览纹理缓存（key = 字体文件路径）。
    pub cache: HashMap<String, egui::TextureHandle>,
    /// 上一帧悬停的字体路径（变化时才发 hover 回调）。
    last_hover: Option<String>,
}


impl FontPickerState {
    /// ctx 切换后（覆盖层每次截图新建 ctx）清空纹理缓存。
    pub fn on_ctx_changed(&mut self) {
        self.cache.clear();
    }
}

/// 选择器外观（调用方按所在界面配色传入，保持苹果风观感一致）。
#[derive(Clone, Copy)]
pub struct PickerStyle {
    /// 按钮底色。
    pub fill: egui::Color32,
    /// 按钮描边。
    pub stroke: egui::Stroke,
    /// 文字/预览着色。
    pub text: egui::Color32,
    /// 弹层背景。
    pub popup_fill: egui::Color32,
    /// 弹层描边。
    pub popup_stroke: egui::Stroke,
}

/// 一帧的选择器结果。
#[derive(Debug, Clone, PartialEq)]
pub enum FontPickOutcome {
    /// 无变化。
    None,
    /// 悬停变化（`None` = 悬停结束/弹层关闭，调用方还原预览；仅变化时发一次）。
    Hover(Option<String>),
    /// 点击提交（`None` = 系统默认；附带清除悬停态，不再发 Hover）。
    Committed(Option<String>),
}

/// 打开选择器弹层并绘制。
#[allow(clippy::too_many_arguments)]
pub fn font_picker_widget(
    ui: &mut egui::Ui,
    id: &str,
    state: &mut FontPickerState,
    style: PickerStyle,
    current_display: &str,
    width: f32,
    show_default_row: bool,
) -> FontPickOutcome {
    let mut committed = None;
    let mut hover_event: Option<Option<String>> = None;
    let button = egui::Button::new(egui::RichText::new(current_display).size(12.5).color(style.text))
        .fill(style.fill)
        .stroke(style.stroke)
        .corner_radius(6.0)
        .min_size(egui::vec2(width, 24.0));
    let btn_resp = ui.add(button);
    if btn_resp.clicked() {
        state.open = !state.open;
    }
    if !state.open {
        if state.last_hover.take().is_some() {
            hover_event = Some(None);
        }
        return outcome(committed, hover_event);
    }

    // 弹层（工具条同款卡片：圆角 + 细描边 + 投影，滚动列表）
    let popup_id = egui::Id::new(format!("font_popup_{id}"));
    let fonts = fontsel::list_fonts();
    let row_h = 26.0_f32;
    let popup_w = 240.0;
    let list_h = ((fonts.len() as f32 + show_default_row as u8 as f32) * row_h + 8.0).min(300.0);
    
    let mut response_map: Vec<(egui::Response, Option<FontChoice>)> = Vec::new();
    let frame_resp = egui::Area::new(popup_id)
        .fixed_pos(egui::pos2(btn_resp.rect.left(), btn_resp.rect.bottom() + 2.0))
        .order(egui::Order::Tooltip)
        .show(ui.ctx(), |ui| {
            egui::Frame::new()
                .fill(style.popup_fill)
                .stroke(style.popup_stroke)
                .shadow(egui::Shadow { offset: [0, 4], blur: 16, spread: 0, color: egui::Color32::from_black_alpha(60) })
                .corner_radius(8.0)
                .inner_margin(egui::Margin::symmetric(4, 4))
                .show(ui, |ui| {
                    ui.set_min_size(egui::vec2(popup_w, list_h));
                    egui::ScrollArea::vertical()
                        .max_height(list_h)
                        .auto_shrink(false)
                        .show(ui, |ui| {
                            if show_default_row {
                                let r = row_ui(ui, popup_w - 8.0, row_h, |ui, _| {
                                    ui.label(egui::RichText::new("系统默认（微软雅黑）").size(12.5).color(style.text));
                                });
                                response_map.push((r, None));
                            }
                            for f in &fonts {
                                let tex = preview_texture(ui.ctx(), &mut state.cache, f);
                                let r = row_ui(ui, popup_w - 8.0, row_h, |ui, _| {
                                    match tex {
                                        Some(t) => {
                                            let img_rect = egui::Rect::from_min_size(
                                                ui.cursor().min,
                                                egui::vec2(190.0, 20.0),
                                            );
                                            ui.allocate_rect(img_rect, egui::Sense::hover());
                                            // 白色字形纹理 tint 当前文字色（深浅主题都清晰）
                                            ui.painter().image(
                                                t.id(),
                                                img_rect,
                                                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                                style.text,
                                            );
                                        }
                                        // 该字体缺显示名字形（预览全透明）→ 名称兜底
                                        None => {
                                            ui.label(egui::RichText::new(&f.display).size(12.5).color(style.text));
                                        }
                                    }
                                });
                                response_map.push((r, Some(f.clone())));
                            }
                        });
                });
        });
    let popup_rect = frame_resp.response.rect;

    // 行交互：悬停高亮 + 回调；点击提交
    for (resp, choice) in response_map {
        let highlighted = resp.hovered();
        let fill = if highlighted {
            ui.visuals().selection.bg_fill
        } else {
            egui::Color32::TRANSPARENT
        };
        ui.painter().rect_filled(resp.rect, 4.0, fill);
        if highlighted {
            let path = choice.as_ref().map(|c| c.path.clone());
            if state.last_hover.as_deref() != path.as_deref() {
                state.last_hover = path.clone();
                hover_event = Some(path);
            }
        }
        if resp.clicked() {
            committed = Some(choice.map(|c| c.path));
            state.open = false;
            state.last_hover = None;
        }
    }

    // 弹层外点击 / Esc 关闭（悬停预览还原经 last_hover 在下方统一处理）
    let pointer_inside = ui
        .ctx()
        .pointer_latest_pos()
        .is_some_and(|p| popup_rect.contains(p) || btn_resp.rect.contains(p));
    let esc = ui.ctx().input(|i| i.key_pressed(egui::Key::Escape));
    let clicked = ui.ctx().input(|i| i.pointer.primary_clicked());
    if esc || (clicked && !pointer_inside) {
        state.open = false;
        if state.last_hover.take().is_some() {
            hover_event = Some(None);
        }
    }
    outcome(committed, hover_event)
}

/// 合成一帧结果：提交优先（提交时不再发悬停结束事件）。
fn outcome(committed: Option<Option<String>>, hover_event: Option<Option<String>>) -> FontPickOutcome {
    if let Some(c) = committed {
        return FontPickOutcome::Committed(c);
    }
    if let Some(h) = hover_event {
        return FontPickOutcome::Hover(h);
    }
    FontPickOutcome::None
}

/// 一行可交互区域（固定尺寸；内容由调用方在返回的 rect 上自绘/加控件）。
fn row_ui(ui: &mut egui::Ui, w: f32, h: f32, content: impl FnOnce(&mut egui::Ui, egui::Rect)) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("row")
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    content(&mut child, rect);
    resp
}

/// 用字体自身渲染显示名 → 纹理（白色字形 + alpha，绘制时 tint 着色适配深浅主题）。
fn preview_texture(
    ctx: &egui::Context,
    cache: &mut HashMap<String, egui::TextureHandle>,
    f: &FontChoice,
) -> Option<egui::TextureHandle> {
    if let Some(t) = cache.get(&f.path) {
        return Some(t.clone());
    }
    let bytes = fontsel::load_font_bytes(&f.path)?;
    // 可解析性检查（不可解析则空白预览 + 名称兜底，且不再重试失败成本极低）
    if ab_glyph::FontRef::try_from_slice_and_index(&bytes, 0).is_err() {
        return None;
    }
    let w = 280u32;
    let h = 36u32;
    let mut img = image::RgbaImage::new(w, h);
    crate::annotation::tools::text::draw_text_in_rect(
        &mut img,
        crate::utils::math::Rect { x: 4, y: 2, width: w - 8, height: h - 4 },
        &f.display,
        crate::annotation::Color { r: 255, g: 255, b: 255, a: 255 },
        22.0,
        false,
        Some(&f.path),
    );
    let color_image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], img.as_raw());
    let handle = ctx.load_texture(format!("font_prev_{}", f.path), color_image, egui::TextureOptions::LINEAR);
    cache.insert(f.path.clone(), handle.clone());
    Some(handle)
}

/// 字体路径 → 显示名（列表内查找；找不到用文件名去扩展名）。
pub fn display_name_for(path: &str) -> String {
    fontsel::list_fonts()
        .iter()
        .find(|f| f.path == path)
        .map(|f| f.display.clone())
        .unwrap_or_else(|| {
            std::path::Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(path)
                .to_string()
        })
}
