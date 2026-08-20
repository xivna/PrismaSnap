//! 标注工具与渲染引擎模块。
//!
//! 渲染一致性方案（AGENTS.md 3.7 节）**已定稿：方案 B（矢量数据 + CPU 重绘）**，
//! 见 PROGRESS.md 决策记录（2026-08-20）：
//! - 标注全程存矢量数据（[`Annotation`] 枚举，**全图物理像素坐标**）；
//! - 预览：egui painter 画在 egui 层（HDR/SDR 两条输出路径天然一致）；
//! - 导出：CPU 光栅器重绘到已色调映射的 sRGB 图上（[`apply_to_image`]，
//!   按选区原点平移坐标），与最终输出同一份像素。
//!
//! 撤销/重做栈见 [`undo_stack`]；各工具的绘制实现见 [`tools`]（Phase 3 逐个落地）。

pub mod tools;
pub mod undo_stack;

use undo_stack::UndoStack;

use crate::utils::math::Rect;

/// 标注颜色（sRGB，RGBA）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const RED: Self = Self::rgb(0xFF, 0x3B, 0x30);
    pub const YELLOW: Self = Self::rgb(0xFF, 0xCC, 0x00);
    pub const GREEN: Self = Self::rgb(0x34, 0xC7, 0x59);
    pub const BLUE: Self = Self::rgb(0x0A, 0x84, 0xFF);
    pub const WHITE: Self = Self::rgb(0xFF, 0xFF, 0xFF);
    pub const BLACK: Self = Self::rgb(0x00, 0x00, 0x00);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
}

/// 标注工具类型（工具条上的可选工具）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// 矩形选框。
    Rect,
    /// 箭头。
    Arrow,
    /// 荧光笔 / 自由划线。
    Brush,
    /// 马赛克 / 模糊。
    Mosaic,
    /// 添加文字。
    Text,
}

impl Tool {
    /// 工具条上展示的全部工具（按展示顺序）。
    pub const ALL: [Tool; 5] = [
        Tool::Rect,
        Tool::Arrow,
        Tool::Brush,
        Tool::Mosaic,
        Tool::Text,
    ];

    /// 工具条按钮标签（图标素材就绪前先用文字按钮）。
    pub fn label(self) -> &'static str {
        match self {
            Tool::Rect => "矩形",
            Tool::Arrow => "箭头",
            Tool::Brush => "画笔",
            Tool::Mosaic => "马赛克",
            Tool::Text => "文字",
        }
    }
}

/// 一条已完成的标注（矢量数据，全图物理像素坐标）。
///
/// 导出时按选区原点平移（`x - sel.x, y - sel.y`），见 [`apply_to_image`]。
#[derive(Debug, Clone, PartialEq)]
pub enum Annotation {
    /// 矩形选框。
    Rect {
        rect: Rect,
        color: Color,
        stroke_width: f32,
    },
    /// 箭头（起点 → 终点）。
    Arrow {
        from: (f32, f32),
        to: (f32, f32),
        color: Color,
        stroke_width: f32,
    },
    /// 荧光笔 / 自由划线（折线点列）。
    Brush {
        points: Vec<(f32, f32)>,
        color: Color,
        stroke_width: f32,
        /// 荧光笔模式：半透明叠加；否则为不透明画笔。
        highlighter: bool,
    },
    /// 马赛克 / 像素化区域。
    Mosaic {
        rect: Rect,
        /// 像素化块边长（物理像素）。
        block_size: u32,
    },
    /// 文字标注。
    Text {
        pos: (f32, f32),
        content: String,
        color: Color,
        font_size: f32,
    },
}

impl Annotation {
    /// 是否为退化标注（拖动距离过小/无有效内容，提交时应丢弃）。
    pub fn is_degenerate(&self) -> bool {
        match self {
            Annotation::Rect { rect, .. } => rect.width < 2 || rect.height < 2,
            Annotation::Arrow { from, to, .. } => (to.0 - from.0).hypot(to.1 - from.1) < 2.0,
            Annotation::Brush { points, .. } => points.len() < 2,
            Annotation::Mosaic { rect, .. } => rect.width < 2 || rect.height < 2,
            Annotation::Text { content, .. } => content.trim().is_empty(),
        }
    }
}

/// 标注管理器：已提交标注（撤销栈）+ 进行中的笔画。
///
/// 编辑态画布把鼠标手势（按下/拖动/释放）翻译成
/// [`begin_stroke`](Self::begin_stroke) / [`update_stroke`](Self::update_stroke) /
/// [`commit_stroke`](Self::commit_stroke) 调用，标注的具体构建规则集中在这里，
/// UI 层不关心各工具的手势差异。
#[derive(Debug)]
pub struct AnnotationManager {
    stack: UndoStack,
    /// 进行中（尚未提交）的标注。
    in_progress: Option<Annotation>,
    /// 笔画起点（拖动锚点，物理像素）。
    stroke_anchor: (f32, f32),
    /// 当前描边颜色（新标注使用）。
    pub stroke_color: Color,
    /// 当前描边宽度（物理像素，新标注使用）。
    pub stroke_width: f32,
}

impl Default for AnnotationManager {
    fn default() -> Self {
        Self {
            stack: UndoStack::new(),
            in_progress: None,
            stroke_anchor: (0.0, 0.0),
            stroke_color: Color::RED,
            stroke_width: 3.0,
        }
    }
}

impl AnnotationManager {
    /// 开始一笔笔画（按下鼠标）。
    ///
    /// 文字工具不走拖动手势（点击放置 + 文本输入，随文字工具任务落地），
    /// 这里不产生进行中标注。
    pub fn begin_stroke(&mut self, tool: Tool, at: (f32, f32)) {
        self.stroke_anchor = at;
        self.in_progress = match tool {
            Tool::Rect => Some(Annotation::Rect {
                rect: Rect::from_points(at.0 as i32, at.1 as i32, at.0 as i32, at.1 as i32),
                color: self.stroke_color,
                stroke_width: self.stroke_width,
            }),
            Tool::Arrow => Some(Annotation::Arrow {
                from: at,
                to: at,
                color: self.stroke_color,
                stroke_width: self.stroke_width,
            }),
            Tool::Brush => Some(Annotation::Brush {
                points: vec![at],
                color: self.stroke_color,
                stroke_width: self.stroke_width,
                highlighter: false,
            }),
            Tool::Mosaic => Some(Annotation::Mosaic {
                rect: Rect::from_points(at.0 as i32, at.1 as i32, at.0 as i32, at.1 as i32),
                block_size: 12,
            }),
            Tool::Text => None,
        };
    }

    /// 更新进行中的笔画（拖动中，物理像素坐标）。
    pub fn update_stroke(&mut self, at: (f32, f32)) {
        let anchor = self.stroke_anchor;
        match &mut self.in_progress {
            Some(Annotation::Rect { rect, .. }) | Some(Annotation::Mosaic { rect, .. }) => {
                *rect = Rect::from_points(anchor.0 as i32, anchor.1 as i32, at.0 as i32, at.1 as i32);
            }
            Some(Annotation::Arrow { to, .. }) => *to = at,
            Some(Annotation::Brush { points, .. }) => points.push(at),
            _ => {}
        }
    }

    /// 提交笔画（释放鼠标）。退化标注（抖动产生的零碎笔画）直接丢弃。
    pub fn commit_stroke(&mut self) {
        if let Some(a) = self.in_progress.take() {
            if !a.is_degenerate() {
                self.stack.push(a);
            }
        }
    }

    /// 放弃进行中的笔画（如 Esc）。
    pub fn cancel_stroke(&mut self) {
        self.in_progress = None;
    }

    /// 撤销最近一次标注。
    pub fn undo(&mut self) -> bool {
        self.stack.undo()
    }

    /// 重做最近一次撤销。
    pub fn redo(&mut self) -> bool {
        self.stack.redo()
    }

    pub fn can_undo(&self) -> bool {
        self.stack.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.stack.can_redo()
    }

    /// 当前生效的已提交标注（绘制顺序）。
    pub fn annotations(&self) -> &[Annotation] {
        self.stack.annotations()
    }

    /// 进行中的标注（预览时与已提交标注一同绘制）。
    pub fn in_progress(&self) -> Option<&Annotation> {
        self.in_progress.as_ref()
    }
}

/// 把标注 CPU 重绘到导出图上（方案 B 导出端）。
///
/// * `img` - 已裁剪的选区图（sRGB）；
/// * `origin` - 选区在全图中的原点（标注坐标为全图物理像素，需平移）。
///
/// Phase 3 骨架阶段：导出管线已接通，各工具的 CPU 绘制随对应工具任务逐个
/// 落地（见 `tools/`）；当前有标注时记录警告并原样返回。
pub fn apply_to_image(img: &mut image::RgbaImage, annotations: &[Annotation], origin: (i32, i32)) {
    if annotations.is_empty() {
        return;
    }
    // TODO(Phase 3): 各工具 CPU 光栅化落地后删除此警告
    tracing::warn!(
        "导出标注 CPU 重绘尚未实现，{} 条标注未写入导出图（origin={origin:?}，img={}x{}）",
        annotations.len(),
        img.width(),
        img.height(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_stroke_commit_and_undo() {
        let mut mgr = AnnotationManager::default();
        mgr.begin_stroke(Tool::Rect, (10.0, 20.0));
        mgr.update_stroke((110.0, 80.0));
        mgr.commit_stroke();

        assert_eq!(mgr.annotations().len(), 1);
        match &mgr.annotations()[0] {
            Annotation::Rect { rect, color, .. } => {
                assert_eq!(rect, &Rect { x: 10, y: 20, width: 100, height: 60 });
                assert_eq!(*color, Color::RED);
            }
            other => panic!("应为矩形标注: {other:?}"),
        }

        assert!(mgr.undo());
        assert!(mgr.annotations().is_empty());
        assert!(mgr.redo());
        assert_eq!(mgr.annotations().len(), 1);
    }

    #[test]
    fn degenerate_stroke_is_discarded() {
        let mut mgr = AnnotationManager::default();
        // 原地点击（拖动距离 0）→ 退化矩形，提交时丢弃
        mgr.begin_stroke(Tool::Rect, (50.0, 50.0));
        mgr.commit_stroke();
        assert!(mgr.annotations().is_empty());
        assert!(!mgr.can_undo());
    }

    #[test]
    fn brush_collects_points() {
        let mut mgr = AnnotationManager::default();
        mgr.begin_stroke(Tool::Brush, (0.0, 0.0));
        mgr.update_stroke((5.0, 5.0));
        mgr.update_stroke((9.0, 3.0));
        mgr.commit_stroke();
        match &mgr.annotations()[0] {
            Annotation::Brush { points, .. } => assert_eq!(points.len(), 3),
            other => panic!("应为画笔标注: {other:?}"),
        }
    }

    #[test]
    fn text_tool_has_no_drag_stroke() {
        let mut mgr = AnnotationManager::default();
        mgr.begin_stroke(Tool::Text, (10.0, 10.0));
        assert!(mgr.in_progress().is_none());
        mgr.commit_stroke();
        assert!(mgr.annotations().is_empty());
    }

    #[test]
    fn annotation_degeneracy_rules() {
        assert!(Annotation::Arrow { from: (0.0, 0.0), to: (1.0, 0.0), color: Color::RED, stroke_width: 2.0 }.is_degenerate());
        assert!(!Annotation::Arrow { from: (0.0, 0.0), to: (10.0, 0.0), color: Color::RED, stroke_width: 2.0 }.is_degenerate());
        assert!(Annotation::Text { pos: (0.0, 0.0), content: "  ".into(), color: Color::RED, font_size: 16.0 }.is_degenerate());
    }
}
