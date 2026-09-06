//! 智能工具条模块。
//!
//! 选区确定后弹出工具条（标注工具 + 撤销/重做 + 复制/保存/取消）。
//!
//! 拆两部分：
//! - **位置计算**（[`toolbar_pos_pts`]）：纯逻辑，跨平台，含单元测试。
//!   优先放选区下方居中，下方空间不足翻到上方，上下都不够放选区内底部；
//!   水平方向钳制在屏幕内（遮挡规避）。
//! - **egui 绘制**（`toolbar_ui`，仅 Windows）：图标按钮（`assets/icons/` 200px
//!   PNG，`include_bytes!` 编进 exe，便携无外部依赖；深色图标配浅色底衬，
//!   深浅主题都看得见），悬停 tooltip 显示中文名；点击结果以 [`ToolbarAction`]
//!   返回给调用方处理。

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

/// 提取文字面板估算尺寸（egui 点：360 内容宽 + 边距，标题栏 + 8 行文本 + 按钮）。
pub const AI_PANEL_SIZE: (f32, f32) = (372.0, 310.0);

/// 提取面板与选区间的间距（egui 点）。
pub const PANEL_GAP: f32 = 8.0;

/// 计算提取文字面板左上角位置（egui 逻辑点）。
///
/// * `sel` - 选区（物理像素）；
/// * `screen` - 显示器物理矩形；
/// * `bar` - 工具条矩形 `(x0, y0, x1, y1)`（egui 点，遮罩挖洞用的同一份缓存），
///   候选位置与其重叠即跳过，保证面板不盖工具条；
/// * `ppp` - 当前 DPI 缩放。
///
/// 优先级：选区右侧 → 左侧 → 下方 → 上方（均须屏内放下且不压工具条）；
/// 都放不下时兜底选区左上内偏移（ historical 行为，钳制屏内）。
/// 右/左候选与选区顶部对齐——面板纵向是文字流，不挡选区正文。
pub fn ai_panel_pos_pts(
    sel: &Rect,
    screen: &Rect,
    bar: Option<(f32, f32, f32, f32)>,
    ppp: f32,
) -> (f32, f32) {
    let (pw, ph) = AI_PANEL_SIZE;
    let sx0 = sel.x as f32 / ppp;
    let sy0 = sel.y as f32 / ppp;
    let sx1 = sel.right() as f32 / ppp;
    let sy1 = sel.bottom() as f32 / ppp;
    let scx0 = screen.x as f32 / ppp;
    let scy0 = screen.y as f32 / ppp;
    let scx1 = screen.right() as f32 / ppp;
    let scy1 = screen.bottom() as f32 / ppp;

    let overlaps_bar = |x: f32, y: f32| match bar {
        None => false,
        Some((bx0, by0, bx1, by1)) => x < bx1 && bx0 < x + pw && y < by1 && by0 < y + ph,
    };
    let fits = |x: f32, y: f32| x >= scx0 && y >= scy0 && x + pw <= scx1 && y + ph <= scy1;

    // 右侧（与选区顶对齐）
    let (rx, ry) = (sx1 + PANEL_GAP, sy0);
    if fits(rx, ry) && !overlaps_bar(rx, ry) {
        return (rx, ry);
    }
    // 左侧
    let (lx, ly) = (sx0 - PANEL_GAP - pw, sy0);
    if fits(lx, ly) && !overlaps_bar(lx, ly) {
        return (lx, ly);
    }
    // 下方（左对齐选区，x 钳制屏内）
    let bx = sx0.clamp(scx0, (scx1 - pw).max(scx0));
    let by = sy1 + PANEL_GAP;
    if fits(bx, by) && !overlaps_bar(bx, by) {
        return (bx, by);
    }
    // 上方
    let ax = bx;
    let ay = sy0 - PANEL_GAP - ph;
    if fits(ax, ay) && !overlaps_bar(ax, ay) {
        return (ax, ay);
    }
    // 兜底：选区左上内偏移（历史行为），钳制屏内
    (
        (sx0 + 6.0).clamp(scx0, (scx1 - pw).max(scx0)),
        (sy0 + 28.0).clamp(scy0, (scy1 - ph).max(scy0)),
    )
}

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

    #[test]
    fn panel_prefers_right_of_selection() {
        // 右侧有空 → 选区右 + gap，顶部对齐
        let sel = Rect { x: 500, y: 300, width: 400, height: 200 };
        assert_eq!(
            ai_panel_pos_pts(&sel, &SCREEN, None, PPP),
            (900.0 + PANEL_GAP, 300.0)
        );
    }

    #[test]
    fn panel_dodges_toolbar_to_left() {
        // 工具条压住右侧候选 → 改走左侧
        let sel = Rect { x: 500, y: 300, width: 400, height: 200 };
        let bar = Some((900.0, 290.0, 1500.0, 400.0));
        assert_eq!(
            ai_panel_pos_pts(&sel, &SCREEN, bar, PPP),
            (500.0 - PANEL_GAP - AI_PANEL_SIZE.0, 300.0)
        );
    }

    #[test]
    fn panel_falls_below_when_sides_blocked() {
        // 左右都放不下（贴边宽选区）→ 下方左对齐
        let sel = Rect { x: 0, y: 300, width: 1900, height: 200 };
        assert_eq!(
            ai_panel_pos_pts(&sel, &SCREEN, None, PPP),
            (0.0, 500.0 + PANEL_GAP)
        );
    }

    #[test]
    fn panel_falls_back_inside_when_nowhere_fits() {
        // 全屏选区哪都放不下 → 兜底左上内偏移
        let sel = Rect { x: 0, y: 0, width: 1920, height: 1080 };
        assert_eq!(ai_panel_pos_pts(&sel, &SCREEN, None, PPP), (6.0, 28.0));
    }
}

// ── egui 绘制（仅 Windows，依赖 egui）────────────────────────────────────

// 图标资源（200×200 RGBA PNG；`include_bytes!` 编进 exe，便携包无外部文件依赖）。
// 命名与按钮一一对应（见 `icon_button`）。
#[cfg(target_os = "windows")]
const ICON_RECT: &[u8] = include_bytes!("../../assets/icons/rect.png");
#[cfg(target_os = "windows")]
const ICON_ARROW: &[u8] = include_bytes!("../../assets/icons/arrow.png");
#[cfg(target_os = "windows")]
const ICON_BRUSH: &[u8] = include_bytes!("../../assets/icons/brush.png");
#[cfg(target_os = "windows")]
const ICON_MOSAIC: &[u8] = include_bytes!("../../assets/icons/mosaic.png");
#[cfg(target_os = "windows")]
const ICON_TEXT: &[u8] = include_bytes!("../../assets/icons/text.png");
#[cfg(target_os = "windows")]
const ICON_UNDO: &[u8] = include_bytes!("../../assets/icons/undo.png");
#[cfg(target_os = "windows")]
const ICON_REDO: &[u8] = include_bytes!("../../assets/icons/redo.png");
#[cfg(target_os = "windows")]
const ICON_COPY: &[u8] = include_bytes!("../../assets/icons/copy.png");
#[cfg(target_os = "windows")]
const ICON_SAVE: &[u8] = include_bytes!("../../assets/icons/save.png");
#[cfg(target_os = "windows")]
const ICON_CANCEL: &[u8] = include_bytes!("../../assets/icons/cancel.png");
#[cfg(target_os = "windows")]
const ICON_EXTRACT: &[u8] = include_bytes!("../../assets/icons/extract.png");
#[cfg(target_os = "windows")]
const ICON_TRANSLATE: &[u8] = include_bytes!("../../assets/icons/translate.png");

/// 图标显示尺寸（egui 点；200px 源图缩下来，HiDPI 也清晰）。
#[cfg(target_os = "windows")]
const ICON_SIZE: f32 = 18.0;

/// 图标按钮底衬（浅色圆角芯片：图标本身是深色线条，深浅主题下都看得见；
/// 选中态改用主题选中色）。
#[cfg(target_os = "windows")]
const ICON_CHIP: egui::Color32 = egui::Color32::from_rgb(240, 240, 243);

/// 取图标纹理（`egui::Context` 数据区缓存，多帧复用不重复解码）。
#[cfg(target_os = "windows")]
fn icon_texture(
    ctx: &egui::Context,
    key: &'static str,
    bytes: &'static [u8],
) -> Option<egui::TextureHandle> {
    let id = egui::Id::new(("toolbar_icon", key));
    if let Some(handle) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return Some(handle);
    }
    let rgba = image::load_from_memory(bytes).ok()?.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    let handle = ctx.load_texture(key, color, egui::TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(id, handle.clone()));
    Some(handle)
}

/// 图标按钮（图标缺失/解码失败时回退文字按钮，保证功能不断）。
///
/// * `selected` - 选中态（当前激活工具）用主题选中色打底；
/// * `enabled` - 禁用态（撤销/重做无历史、AI 忙时）；
/// * 返回 `(是否被点, 响应)`——调用方照旧处理点击，tooltip 已内置。
#[cfg(target_os = "windows")]
fn icon_button(
    ui: &mut egui::Ui,
    tooltip: &str,
    key: &'static str,
    bytes: &'static [u8],
    selected: bool,
    enabled: bool,
) -> egui::Response {
    let fill = if selected {
        ui.visuals().selection.bg_fill
    } else {
        ICON_CHIP
    };
    let Some(handle) = icon_texture(ui.ctx(), key, bytes) else {
        return ui.add_enabled(enabled, egui::Button::new(tooltip).fill(fill));
    };
    let img = egui::Image::new(&handle).fit_to_exact_size(egui::vec2(ICON_SIZE, ICON_SIZE));
    let btn = egui::Button::image(img)
        .fill(fill)
        .min_size(egui::vec2(ICON_SIZE + 12.0, ICON_SIZE + 8.0));
    ui.add_enabled(enabled, btn).on_hover_text(tooltip)
}

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
    /// OCR 全量识别选区文字（结果进可编辑面板）。
    ExtractText,
    /// 按配置模式翻译并原位覆盖（见 AGENTS.md 3.8 节）。
    Translate,
}

/// 绘制工具条，返回本帧被点击的动作（无点击返回 `None`）。
///
/// * `pos` - 左上角（egui 逻辑点，由 [`toolbar_pos_pts`] 计算）；
/// * `active_tool` - 当前激活的标注工具（高亮显示）;
/// * `stroke_color` / `stroke_width` - 当前颜色与线宽（第二行选中高亮）；
/// * `can_undo` / `can_redo` - 撤销/重做按钮可用状态；
/// * `ai_busy` - AI 任务（识别/翻译）进行中时禁用提取/翻译按钮，防重复提交；
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
    ai_busy: bool,
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
                            // 标注工具（图标按钮，悬停看中文名；激活态主题色打底）
                            const TOOL_ICONS: [(crate::annotation::Tool, &str, &[u8]); 5] = [
                                (Tool::Rect, "矩形", ICON_RECT),
                                (Tool::Arrow, "箭头", ICON_ARROW),
                                (Tool::Brush, "画笔", ICON_BRUSH),
                                (Tool::Mosaic, "马赛克", ICON_MOSAIC),
                                (Tool::Text, "文字", ICON_TEXT),
                            ];
                            for (tool, name, bytes) in TOOL_ICONS {
                                let key = match tool {
                                    Tool::Rect => "rect",
                                    Tool::Arrow => "arrow",
                                    Tool::Brush => "brush",
                                    Tool::Mosaic => "mosaic",
                                    Tool::Text => "text",
                                };
                                if icon_button(ui, name, key, bytes, active_tool == Some(tool), true)
                                    .clicked()
                                {
                                    action = Some(ToolbarAction::ActivateTool(tool));
                                }
                            }
                            ui.separator();
                            if icon_button(ui, "撤销", "undo", ICON_UNDO, false, can_undo)
                                .clicked()
                            {
                                action = Some(ToolbarAction::Undo);
                            }
                            if icon_button(ui, "重做", "redo", ICON_REDO, false, can_redo)
                                .clicked()
                            {
                                action = Some(ToolbarAction::Redo);
                            }
                            ui.separator();
                            if icon_button(ui, "复制", "copy", ICON_COPY, false, true).clicked()
                            {
                                action = Some(ToolbarAction::Copy);
                            }
                            if icon_button(ui, "保存", "save", ICON_SAVE, false, true).clicked()
                            {
                                action = Some(ToolbarAction::Save);
                            }
                            if icon_button(ui, "取消", "cancel", ICON_CANCEL, false, true)
                                .clicked()
                            {
                                action = Some(ToolbarAction::Cancel);
                            }
                            ui.separator();
                            if icon_button(
                                ui,
                                "提取文字",
                                "extract",
                                ICON_EXTRACT,
                                false,
                                !ai_busy,
                            )
                            .clicked()
                            {
                                action = Some(ToolbarAction::ExtractText);
                            }
                            if icon_button(
                                ui,
                                "翻译",
                                "translate",
                                ICON_TRANSLATE,
                                false,
                                !ai_busy,
                            )
                            .clicked()
                            {
                                action = Some(ToolbarAction::Translate);
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
