//! 智能工具条模块。
//!
//! 选区确定后弹出工具条（标注工具 + 撤销/重做 + 复制/保存/取消）。
//!
//! 拆两部分：
//! - **位置计算**（[`toolbar_pos_pts`]）：纯逻辑，跨平台，含单元测试。
//!   优先放选区下方居中，下方空间不足翻到上方，上下都不够放选区内底部；
//!   水平方向钳制在屏幕内（遮挡规避）。
//! - **egui 绘制**（`toolbar_ui`，仅 Windows）：文字按钮占位（图标素材
//!   用户准备中），点击结果以 [`ToolbarAction`] 返回给调用方处理。

#[cfg(target_os = "windows")]
use crate::annotation::Color;
use crate::utils::math::Rect;

/// 工具条估算尺寸（egui 点）。
///
/// 两/三行布局：第一行工具按钮；第二行颜色/线宽或遮挡样式；马赛克像素化/模糊时
/// 额外第三行滑块。按最大高度估算（110pt），保证位置计算不遮挡。
pub const BAR_SIZE: (f32, f32) = (600.0, 110.0);

/// 工具条与选区间的间距（egui 点）。
pub const BAR_GAP: f32 = 10.0;

/// 工具条卡片圆角（egui 点）。覆盖层遮罩挖洞需与此保持一致，
/// 保证洞的圆角与工具条卡片完全重合（见 `overlay::dim_outside_selection`）。
pub const CORNER_RADIUS: f32 = 10.0;

/// 预设标注颜色（展示顺序，取自 [`Color`] 常量）。
#[cfg(target_os = "windows")]
const PRESET_COLORS: [Color; 6] = [
    Color::RED,
    Color::YELLOW,
    Color::GREEN,
    Color::BLUE,
    Color::WHITE,
    Color::BLACK,
];

/// 线宽档位 `(线宽物理像素, 按钮圆点字号 pt)`。
#[cfg(target_os = "windows")]
const WIDTH_STEPS: [(f32, f32); 3] = [(3.0, 10.0), (6.0, 13.0), (10.0, 16.0)];

/// 计算工具条左上角位置（egui 逻辑点）。
///
/// * `sel` - 选区（物理像素）；
/// * `screen` - 显示器物理矩形；
/// * `bar_size` - 工具条估算尺寸（egui 点）；
/// * `ppp` - 当前 DPI 缩放（pixels per point）。
///
/// 纵向：优先选区下方 → 上方 → 选区内底部（选区几乎占满全屏时）；
/// 横向：选区水平居中，钳制在屏幕内（贴边留 4pt）。
pub fn toolbar_pos_pts(sel: &Rect, screen: &Rect, bar_size: (f32, f32), ppp: f32) -> (f32, f32) {
    let (bar_w, bar_h) = bar_size;
    // 物理 → 逻辑点
    let sx0 = sel.x as f32 / ppp;
    let sy0 = sel.y as f32 / ppp;
    let sx1 = sel.right() as f32 / ppp;
    let sy1 = sel.bottom() as f32 / ppp;
    let scx0 = screen.x as f32 / ppp;
    let scy0 = screen.y as f32 / ppp;
    let scx1 = screen.right() as f32 / ppp;
    let scy1 = screen.bottom() as f32 / ppp;

    let y = if sy1 + BAR_GAP + bar_h <= scy1 {
        // 选区下方
        sy1 + BAR_GAP
    } else if sy0 - BAR_GAP - bar_h >= scy0 {
        // 选区上方
        sy0 - BAR_GAP - bar_h
    } else {
        // 上下都不够：放选区内底部
        (sy1 - BAR_GAP - bar_h).max(scy0)
    };
    let x = ((sx0 + sx1) / 2.0 - bar_w / 2.0).clamp(scx0 + 4.0, scx1 - bar_w - 4.0);
    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect { x: 0, y: 0, width: 1920, height: 1080 };
    const PPP: f32 = 1.0;

    fn bar() -> (f32, f32) {
        (600.0, 44.0)
    }

    #[test]
    fn prefers_below_selection_centered() {
        let sel = Rect { x: 500, y: 300, width: 400, height: 200 };
        let (x, y) = toolbar_pos_pts(&sel, &SCREEN, bar(), PPP);
        // 下方：y = 选区底 + gap
        assert_eq!(y, 500.0 + BAR_GAP);
        // 水平居中于选区：选区中心 700 - 条宽一半 300 = 400
        assert_eq!(x, 400.0);
    }

    #[test]
    fn flips_above_when_no_space_below() {
        let sel = Rect { x: 500, y: 900, width: 400, height: 160 }; // 底 1060，下方放不下
        let (x, y) = toolbar_pos_pts(&sel, &SCREEN, bar(), PPP);
        // 上方：y = 选区顶 - gap - 条高
        assert_eq!(y, 900.0 - BAR_GAP - 44.0);
        assert_eq!(x, 400.0);
    }

    #[test]
    fn inside_selection_when_full_height() {
        let sel = Rect { x: 0, y: 0, width: 1920, height: 1080 };
        let (_, y) = toolbar_pos_pts(&sel, &SCREEN, bar(), PPP);
        // 上下都不够 → 选区内底部：1080 - gap - 44
        assert_eq!(y, 1080.0 - BAR_GAP - 44.0);
    }

    #[test]
    fn clamps_horizontally_to_screen() {
        // 贴左边缘的窄选区 → 条不能伸出左边界
        let sel = Rect { x: 0, y: 300, width: 100, height: 100 };
        let (x, _) = toolbar_pos_pts(&sel, &SCREEN, bar(), PPP);
        assert_eq!(x, 4.0);
        // 贴右边缘 → 条不超出右边界
        let sel = Rect { x: 1820, y: 300, width: 100, height: 100 };
        let (x, _) = toolbar_pos_pts(&sel, &SCREEN, bar(), PPP);
        assert_eq!(x, 1920.0 - 600.0 - 4.0);
    }

    #[test]
    fn respects_dpi_scaling() {
        // 150% 缩放：物理坐标 ÷ 1.5 = 逻辑点
        let sel = Rect { x: 750, y: 450, width: 600, height: 300 };
        let screen = Rect { x: 0, y: 0, width: 2880, height: 1620 };
        let (x, y) = toolbar_pos_pts(&sel, &screen, bar(), 1.5);
        assert_eq!(y, (450.0 + 300.0) / 1.5 + BAR_GAP);
        // 选区中心点 = (750+300)/1.5 = 700，x = 700 - 300 = 400
        assert_eq!(x, 400.0);
    }
}

// ── egui 绘制（仅 Windows，依赖 egui）────────────────────────────────────

/// 工具条点击结果（由覆盖层在处理完渲染后统一响应）。
#[cfg(target_os = "windows")]
#[derive(Debug, Clone, PartialEq)]
pub enum ToolbarAction {
    /// 激活/切换标注工具。
    ActivateTool(crate::annotation::Tool),
    /// 切换当前描边颜色（对新标注生效）。
    SetColor(Color),
    /// 切换当前描边宽度（对新标注生效）。
    SetStrokeWidth(f32),
    /// 切换文字字号。
    SetTextFontSize(f32),
    /// 切换文字是否加粗。
    SetTextBold(bool),
    /// 切换遮挡样式（马赛克工具）。
    SetMosaicStyle(crate::annotation::MosaicStyle),
    Undo,
    Redo,
    Copy,
    Save,
    Cancel,
}

/// 绘制工具条，返回本帧被点击的动作（无点击返回 `None`）。
///
/// * `pos` - 左上角（egui 逻辑点，由 [`toolbar_pos_pts`] 计算）；
/// * `active_tool` - 当前激活的标注工具（高亮显示）;
/// * `stroke_color` / `stroke_width` - 当前颜色与线宽（第二行选中高亮）；
/// * `can_undo` / `can_redo` - 撤销/重做按钮可用状态；
/// * `actual_rect` - 输出本帧工具条的**实际**渲染矩形（egui 逻辑点），
///   调用方用于遮罩挖洞（[`crate::ui::overlay`]），与估算尺寸 [`BAR_SIZE`]
///   相比这才是真实边界。
#[cfg(target_os = "windows")]
pub fn toolbar_ui(
    ctx: &egui::Context,
    pos: (f32, f32),
    active_tool: Option<crate::annotation::Tool>,
    stroke_color: Color,
    stroke_width: f32,
    mosaic_style: &crate::annotation::MosaicStyle,
    text_font_size: f32,
    text_bold: bool,
    can_undo: bool,
    can_redo: bool,
    actual_rect: &mut Option<egui::Rect>,
) -> Option<ToolbarAction> {
    use crate::annotation::Tool;

    let mut action = None;
    let area_response = egui::Area::new(egui::Id::new("prismsnap_toolbar"))
        .fixed_pos(egui::pos2(pos.0, pos.1))
        .order(egui::Order::Tooltip)
        .show(ctx, |ui| {
            // 浮层风格跟随界面主题（设置界面「主题」项即时生效）：
            // 浅色 = 白卡片 + 投影；深色 = 深灰卡片 + 白描边。
            // 柔和投影是浮层质感的关键——人眼识别"悬浮"靠阴影而非绝对亮度；
            // 子树 visuals 同步切换，保证文字/控件与底色协调
            let dark_mode = ui.visuals().dark_mode;
            ui.style_mut().visuals = if dark_mode {
                egui::Visuals::dark()
            } else {
                egui::Visuals::light()
            };
            let (fill, frame_stroke) = if dark_mode {
                (
                    egui::Color32::from_rgba_unmultiplied(44, 44, 50, 244),
                    egui::Color32::from_white_alpha(30),
                )
            } else {
                (
                    egui::Color32::from_rgba_unmultiplied(250, 250, 252, 248),
                    egui::Color32::from_black_alpha(28),
                )
            };
            egui::Frame::new()
                .fill(fill)
                .stroke(egui::Stroke::new(1.0, frame_stroke))
                .shadow(egui::Shadow {
                    offset: [0, 4],
                    blur: 18,
                    spread: 0,
                    color: egui::Color32::from_black_alpha(70),
                })
                .corner_radius(CORNER_RADIUS)
                .inner_margin(egui::Margin::symmetric(8, 6))
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            for tool in Tool::ALL {
                                let mut btn = egui::Button::new(tool.label());
                                if active_tool == Some(tool) {
                                    btn = btn.fill(ui.visuals().selection.bg_fill);
                                }
                                if ui.add(btn).clicked() {
                                    action = Some(ToolbarAction::ActivateTool(tool));
                                }
                            }
                            ui.separator();
                            if ui.add_enabled(can_undo, egui::Button::new("撤销")).clicked() {
                                action = Some(ToolbarAction::Undo);
                            }
                            if ui.add_enabled(can_redo, egui::Button::new("重做")).clicked() {
                                action = Some(ToolbarAction::Redo);
                            }
                            ui.separator();
                            if ui.button("复制").clicked() {
                                action = Some(ToolbarAction::Copy);
                            }
                            if ui.button("保存").clicked() {
                                action = Some(ToolbarAction::Save);
                            }
                            if ui.button("取消").clicked() {
                                action = Some(ToolbarAction::Cancel);
                            }
                        });
                        // 第二行：马赛克显示样式切换+参数，其它工具显示颜色+线宽
                        ui.add_space(2.0);
                        if active_tool == Some(Tool::Mosaic) {
                            // 样式切换
                            ui.horizontal(|ui| {
                                let is_pixel = matches!(mosaic_style, crate::annotation::MosaicStyle::Pixelate { .. });
                                let is_blur = matches!(mosaic_style, crate::annotation::MosaicStyle::Blur { .. });
                                let is_solid = matches!(mosaic_style, crate::annotation::MosaicStyle::Solid { .. });
                                let mut b1 = egui::Button::new("像素化");
                                if is_pixel { b1 = b1.fill(ui.visuals().selection.bg_fill); }
                                if ui.add(b1).clicked() {
                                    action = Some(ToolbarAction::SetMosaicStyle(crate::annotation::MosaicStyle::Pixelate { block_size: 18 }));
                                }
                                let mut b2 = egui::Button::new("模糊");
                                if is_blur { b2 = b2.fill(ui.visuals().selection.bg_fill); }
                                if ui.add(b2).clicked() {
                                    action = Some(ToolbarAction::SetMosaicStyle(crate::annotation::MosaicStyle::Blur { radius: 12.0 }));
                                }
                                let mut b3 = egui::Button::new("纯色");
                                if is_solid { b3 = b3.fill(ui.visuals().selection.bg_fill); }
                                if ui.add(b3).clicked() {
                                    action = Some(ToolbarAction::SetMosaicStyle(crate::annotation::MosaicStyle::Solid { color: stroke_color }));
                                }
                            });
                            // 参数行（块大小 / 模糊半径 / 纯色颜色）
                            ui.add_space(2.0);
                            ui.horizontal(|ui| {
                                match mosaic_style {
                                    crate::annotation::MosaicStyle::Pixelate { block_size } => {
                                        let mut bs = *block_size as i32;
                                        let resp = ui.add(egui::Slider::new(&mut bs, 4..=40).text("块大小"));
                                        if resp.changed() {
                                            action = Some(ToolbarAction::SetMosaicStyle(crate::annotation::MosaicStyle::Pixelate { block_size: bs as u32 }));
                                        }
                                    }
                                    crate::annotation::MosaicStyle::Blur { radius } => {
                                        let mut r = *radius;
                                        let resp = ui.add(egui::Slider::new(&mut r, 2.0..=30.0).text("模糊"));
                                        if resp.changed() {
                                            action = Some(ToolbarAction::SetMosaicStyle(crate::annotation::MosaicStyle::Blur { radius: r }));
                                        }
                                    }
                                    crate::annotation::MosaicStyle::Solid { .. } => {
                                        // 纯色共享统一颜色（与绘制图形同一调色板）
                                        for &c in &PRESET_COLORS {
                                            let selected = stroke_color == c;
                                            let stroke = if selected {
                                                egui::Stroke::new(2.5, ui.visuals().strong_text_color())
                                            } else {
                                                egui::Stroke::new(1.0, egui::Color32::from_black_alpha(50))
                                            };
                                            let btn = egui::Button::new("")
                                                .fill(egui::Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a))
                                                .stroke(stroke)
                                                .min_size(egui::vec2(20.0, 20.0))
                                                .corner_radius(10.0);
                                            if ui.add(btn).clicked() {
                                                // 同步更新统一颜色与纯色遮挡颜色
                                                action = Some(ToolbarAction::SetMosaicStyle(crate::annotation::MosaicStyle::Solid { color: c }));
                                            }
                                        }
                                    }
                                }
                            });
                        } else if active_tool == Some(Tool::Text) {
                            ui.horizontal(|ui| {
                                for &c in &PRESET_COLORS {
                                    let selected = stroke_color == c;
                                    let stroke = if selected {
                                        egui::Stroke::new(2.5, ui.visuals().strong_text_color())
                                    } else {
                                        egui::Stroke::new(1.0, egui::Color32::from_black_alpha(50))
                                    };
                                    let btn = egui::Button::new("")
                                        .fill(egui::Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a))
                                        .stroke(stroke)
                                        .min_size(egui::vec2(20.0, 20.0))
                                        .corner_radius(10.0);
                                    if ui.add(btn).clicked() {
                                        action = Some(ToolbarAction::SetColor(c));
                                    }
                                }
                                ui.separator();
                                let mut sz = text_font_size;
                                let resp = ui.add(egui::Slider::new(&mut sz, 8.0..=120.0).text("字号"));
                                if resp.changed() {
                                    action = Some(ToolbarAction::SetTextFontSize(sz));
                                }
                                let mut bold = text_bold;
                                if ui.checkbox(&mut bold, "加粗").changed() {
                                    action = Some(ToolbarAction::SetTextBold(bold));
                                }
                            });
                        } else {
                            ui.horizontal(|ui| {
                                for &c in &PRESET_COLORS {
                                    let selected = stroke_color == c;
                                    let stroke = if selected {
                                        egui::Stroke::new(2.5, ui.visuals().strong_text_color())
                                    } else {
                                        egui::Stroke::new(1.0, egui::Color32::from_black_alpha(50))
                                    };
                                    let btn = egui::Button::new("")
                                        .fill(egui::Color32::from_rgba_unmultiplied(
                                            c.r, c.g, c.b, c.a,
                                        ))
                                        .stroke(stroke)
                                        .min_size(egui::vec2(20.0, 20.0))
                                        .corner_radius(10.0);
                                    if ui.add(btn).clicked() {
                                        action = Some(ToolbarAction::SetColor(c));
                                    }
                                }
                                ui.separator();
                                for &(w, dot) in &WIDTH_STEPS {
                                    let selected = (stroke_width - w).abs() < f32::EPSILON;
                                    let mut btn =
                                        egui::Button::new(egui::RichText::new("●").size(dot));
                                    if selected {
                                        btn = btn.fill(ui.visuals().selection.bg_fill);
                                    }
                                    if ui.add(btn).clicked() {
                                        action = Some(ToolbarAction::SetStrokeWidth(w));
                                    }
                                }
                            });
                        }
                    });
                });
        });
    // 记录本帧 Area 的实际边界（含 Frame 内边距），供遮罩精确挖洞
    *actual_rect = Some(area_response.response.rect);
    action
}
