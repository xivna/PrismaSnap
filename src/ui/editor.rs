//! 标注编辑器模块（仅 Windows 平台编译）。
//!
//! 选区确定后的无边框浮窗编辑态，提供标注画布与工具条。
//! 与覆盖层共用同一窗口，通过显示模式切换（单窗口切换架构）。
//!
//! 职责：持有标注数据（[`AnnotationManager`]）与当前激活工具，
//! 把覆盖层转发来的鼠标手势翻译成笔画构建调用，并在 egui 层绘制
//! 标注预览（方案 B：预览走 egui painter，导出由 CPU 光栅器重绘，
//! 见 `annotation/mod.rs` 模块文档）。

use crate::annotation::{Annotation, AnnotationManager, Color, Tool};

/// 标注编辑器状态。
pub struct Editor {
    mgr: AnnotationManager,
    /// 当前激活的标注工具（`None` = 未进入编辑态）。
    active_tool: Option<Tool>,
}

impl Editor {
    /// 创建空编辑器（无激活工具）。
    pub fn new() -> Self {
        Self {
            mgr: AnnotationManager::default(),
            active_tool: None,
        }
    }

    /// 当前激活工具。
    pub fn active_tool(&self) -> Option<Tool> {
        self.active_tool
    }

    /// 是否处于编辑态（有激活工具）。
    pub fn is_editing(&self) -> bool {
        self.active_tool.is_some()
    }

    /// 是否有进行中的笔画（按下未释放）。
    pub fn is_stroking(&self) -> bool {
        self.mgr.in_progress().is_some()
    }

    /// 激活工具（进入编辑态）。
    pub fn activate(&mut self, tool: Tool) {
        self.active_tool = Some(tool);
    }

    /// 退出编辑态（回到预览）。进行中的笔画丢弃。
    pub fn deactivate(&mut self) {
        self.active_tool = None;
        self.mgr.cancel_stroke();
    }

    /// 按下左键：按当前工具开始一笔（无激活工具时不动作）。
    pub fn begin_stroke(&mut self, at: (f32, f32)) {
        if let Some(tool) = self.active_tool {
            self.mgr.begin_stroke(tool, at);
        }
    }

    /// 拖动中：更新进行中的笔画。
    pub fn update_stroke(&mut self, at: (f32, f32)) {
        self.mgr.update_stroke(at);
    }

    /// 释放左键：提交笔画（退化标注自动丢弃）。
    pub fn commit_stroke(&mut self) {
        self.mgr.commit_stroke();
    }

    /// 撤销最近一次标注。
    pub fn undo(&mut self) -> bool {
        self.mgr.undo()
    }

    /// 重做最近一次撤销。
    pub fn redo(&mut self) -> bool {
        self.mgr.redo()
    }

    pub fn can_undo(&self) -> bool {
        self.mgr.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.mgr.can_redo()
    }

    /// 当前生效的已提交标注（导出时交给 CPU 光栅器重绘）。
    pub fn annotations(&self) -> &[Annotation] {
        self.mgr.annotations()
    }

    /// 在 egui 层绘制全部标注预览（已提交 + 进行中）。
    ///
    /// 坐标转换：标注存全图物理像素，egui 绘制 ÷ ppp 转逻辑点。
    /// 骨架阶段各工具统一以简单形状预览（马赛克为半透明色块占位，
    /// 文字工具暂不预览），导出端 CPU 绘制随各工具任务逐个落地。
    pub fn draw_annotations(&self, painter: &egui::Painter, ppp: f32) {
        for ann in self.mgr.annotations().iter().chain(self.mgr.in_progress()) {
            draw_annotation(painter, ann, ppp);
        }
    }
}

/// 单条标注的 egui 预览绘制。
fn draw_annotation(painter: &egui::Painter, ann: &Annotation, ppp: f32) {
    let to_pt = |p: (f32, f32)| egui::pos2(p.0 / ppp, p.1 / ppp);
    let stroke = |color: Color, w: f32| {
        egui::Stroke::new(w / ppp, egui::Color32::from_rgba_unmultiplied(color.r, color.g, color.b, color.a))
    };
    match ann {
        Annotation::Rect { rect, color, stroke_width } => {
            let r = egui::Rect::from_min_max(
                to_pt((rect.x as f32, rect.y as f32)),
                to_pt((rect.right() as f32, rect.bottom() as f32)),
            );
            painter.rect_stroke(r, 0.0, stroke(*color, *stroke_width), egui::StrokeKind::Outside);
        }
        Annotation::Arrow { from, to, color, stroke_width } => {
            let (from, to) = (to_pt(*from), to_pt(*to));
            let st = stroke(*color, *stroke_width);
            painter.line_segment([from, to], st);
            // 箭头头部：终点处两条 30° 夹角短线
            let dir = (to - from).normalized();
            let head_len = 12.0 * st.width.max(1.0);
            for angle in [std::f32::consts::PI * 5.0 / 6.0, -std::f32::consts::PI * 5.0 / 6.0] {
                let (sin, cos) = angle.sin_cos();
                let d = egui::vec2(dir.x * cos - dir.y * sin, dir.x * sin + dir.y * cos);
                painter.line_segment([to, to - d * head_len], st);
            }
        }
        Annotation::Brush { points, color, stroke_width, highlighter } => {
            let pts: Vec<egui::Pos2> = points.iter().map(|&p| to_pt(p)).collect();
            if pts.len() >= 2 {
                let mut st = stroke(*color, *stroke_width);
                if *highlighter {
                    st.color = egui::Color32::from_rgba_unmultiplied(color.r, color.g, color.b, 110);
                }
                painter.add(egui::Shape::line(pts, st));
            }
        }
        Annotation::Mosaic { rect, .. } => {
            // 占位预览：半透明灰色块（导出端像素化随马赛克工具任务落地）
            let r = egui::Rect::from_min_max(
                to_pt((rect.x as f32, rect.y as f32)),
                to_pt((rect.right() as f32, rect.bottom() as f32)),
            );
            painter.rect_filled(r, 0.0, egui::Color32::from_black_alpha(120));
            painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)), egui::StrokeKind::Outside);
        }
        Annotation::Text { .. } => {
            // 文字标注预览随文字工具任务落地（需 ab_glyph 量测与 IME 输入）
        }
    }
}
