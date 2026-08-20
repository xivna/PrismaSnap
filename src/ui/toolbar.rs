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

use crate::utils::math::Rect;

/// 工具条估算尺寸（egui 点）。
///
/// 10 个文字按钮（5 工具 + 撤销/重做 + 复制/保存/取消，每个约 54pt）
/// + 分隔符与内边距。仅用于弹出前的位置计算（需先知尺寸才能定位），
/// 与实际渲染尺寸的小偏差不影响正确性（水平钳制留了余量）。
pub const BAR_SIZE: (f32, f32) = (600.0, 44.0);

/// 工具条与选区间的间距（egui 点）。
pub const BAR_GAP: f32 = 10.0;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolbarAction {
    /// 激活/切换标注工具。
    ActivateTool(crate::annotation::Tool),
    Undo,
    Redo,
    Copy,
    Save,
    Cancel,
}

/// 绘制工具条，返回本帧被点击的动作（无点击返回 `None`）。
///
/// * `pos` - 左上角（egui 逻辑点，由 [`toolbar_pos_pts`] 计算）；
/// * `active_tool` - 当前激活的标注工具（高亮显示）；
/// * `can_undo` / `can_redo` - 撤销/重做按钮可用状态。
#[cfg(target_os = "windows")]
pub fn toolbar_ui(
    ctx: &egui::Context,
    pos: (f32, f32),
    active_tool: Option<crate::annotation::Tool>,
    can_undo: bool,
    can_redo: bool,
) -> Option<ToolbarAction> {
    use crate::annotation::Tool;

    let mut action = None;
    egui::Area::new(egui::Id::new("prismsnap_toolbar"))
        .fixed_pos(egui::pos2(pos.0, pos.1))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            // 填充/描边跟随当前主题（深色工具条文字辨识度差，见配置 ui.theme）
            egui::Frame::new()
                .fill(ui.visuals().window_fill())
                .stroke(ui.visuals().window_stroke())
                .corner_radius(8.0)
                .inner_margin(egui::Margin::symmetric(8, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for tool in Tool::ALL {
                            let mut btn = egui::Button::new(tool.label());
                            if active_tool == Some(tool) {
                                btn = btn.fill(egui::Color32::from_rgb(0, 110, 200));
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
                });
        });
    action
}
