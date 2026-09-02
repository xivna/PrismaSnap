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
/// 每条标注带稳定 `id: u64`，自增不复用，用于预览缓存 Key（避免数组下标复用导致旧纹理残留）。
#[derive(Debug, Clone, PartialEq)]
pub enum Annotation {
    /// 矩形选框。
    Rect {
        id: u64,
        rect: Rect,
        color: Color,
        stroke_width: f32,
    },
    /// 箭头（起点 → 终点）。
    Arrow {
        id: u64,
        from: (f32, f32),
        to: (f32, f32),
        color: Color,
        stroke_width: f32,
    },
    /// 荧光笔 / 自由划线（折线点列）。
    Brush {
        id: u64,
        points: Vec<(f32, f32)>,
        color: Color,
        stroke_width: f32,
        /// 荧光笔模式：半透明叠加；否则为不透明画笔。
        highlighter: bool,
    },
    /// 遮挡（马赛克/模糊/纯色等，复用同一矩形选区）。
    Mosaic {
        id: u64,
        rect: Rect,
        /// 遮挡样式（像素化/模糊/纯色）。
        style: MosaicStyle,
    },
    /// 文字标注（矩形文本框，可拖动缩放；`rect` 为物理像素文本框）。
    Text {
        id: u64,
        rect: Rect,
        content: String,
        color: Color,
        font_size: f32,
        /// 是否加粗（预览用 egui 粗体，导出用描边模拟或粗体字体）。
        bold: bool,
    },
}

impl Annotation {
    /// 稳定 id（不因 delete/undo 复用）。
    pub fn id(&self) -> u64 {
        match self {
            Annotation::Rect { id, .. }
            | Annotation::Arrow { id, .. }
            | Annotation::Brush { id, .. }
            | Annotation::Mosaic { id, .. }
            | Annotation::Text { id, .. } => *id,
        }
    }
}

/// 马赛克/遮挡样式（对应外部评审方案二）。
#[derive(Debug, Clone, PartialEq)]
pub enum MosaicStyle {
    /// 像素化（块均值，默认 18）。
    Pixelate { block_size: u32 },
    /// 高斯模糊（sigma）。
    Blur { radius: f32 },
    /// 纯色遮挡（不透明填充）。
    Solid { color: Color },
}

impl Default for MosaicStyle {
    fn default() -> Self {
        Self::Pixelate { block_size: 18 }
    }
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

    /// 物理像素包围盒（含描边外扩，用于命中与拖动边界判断）。
    pub fn bounds(&self) -> Rect {
        match self {
            Annotation::Rect { rect, stroke_width, .. } => {
                let w = (*stroke_width as i32).max(1);
                Rect { x: rect.x - w, y: rect.y - w, width: rect.width + 2 * w as u32, height: rect.height + 2 * w as u32 }
            }
            Annotation::Arrow { from, to, stroke_width, .. } => {
                let w = (*stroke_width as i32).max(1);
                let min_x = from.0.min(to.0) as i32 - w - 8;
                let min_y = from.1.min(to.1) as i32 - w - 8;
                let max_x = from.0.max(to.0) as i32 + w + 8;
                let max_y = from.1.max(to.1) as i32 + w + 8;
                let width = (max_x - min_x).max(1) as u32;
                let height = (max_y - min_y).max(1) as u32;
                Rect { x: min_x, y: min_y, width, height }
            }
            Annotation::Brush { points, stroke_width, .. } => {
                if points.is_empty() {
                    return Rect { x: 0, y: 0, width: 0, height: 0 };
                }
                let w = (*stroke_width as i32).max(1);
                let mut min_x = points[0].0 as i32;
                let mut min_y = points[0].1 as i32;
                let mut max_x = min_x;
                let mut max_y = min_y;
                for &(x, y) in points.iter().skip(1) {
                    min_x = min_x.min(x as i32);
                    min_y = min_y.min(y as i32);
                    max_x = max_x.max(x as i32);
                    max_y = max_y.max(y as i32);
                }
                Rect { x: min_x - w, y: min_y - w, width: (max_x - min_x + 2 * w) as u32, height: (max_y - min_y + 2 * w) as u32 }
            }
            Annotation::Mosaic { rect, style: _, .. } => *rect,
            Annotation::Text { rect, .. } => *rect,
        }
    }

    /// 命中测试（物理像素点是否落在标注可拖动区域内）。
    ///
    /// 内置容差带：矩形/马赛克为包围盒（含描边）；箭头为到线段距离；
    /// 画笔为到折线各段距离；文字为包围盒。
    pub fn hit_test(&self, pt: (f32, f32)) -> bool {
        const TOL: f32 = 6.0;
        match self {
            Annotation::Rect { rect, stroke_width, .. } => {
                let w = (*stroke_width).max(1.0);
                let outer = Rect { x: rect.x - w as i32 - TOL as i32, y: rect.y - w as i32 - TOL as i32, width: rect.width + 2 * (w as u32 + TOL as u32), height: rect.height + 2 * (w as u32 + TOL as u32) };
                // 扩大包围盒内即视为命中（拖动友好，含内部）
                let r = outer;
                let x = pt.0 as i32;
                let y = pt.1 as i32;
                x >= r.x && x < r.right() && y >= r.y && y < r.bottom()
            }
            Annotation::Mosaic { rect, .. } => {
                let r = Rect { x: rect.x - TOL as i32, y: rect.y - TOL as i32, width: rect.width + 2 * TOL as u32, height: rect.height + 2 * TOL as u32 };
                let x = pt.0 as i32;
                let y = pt.1 as i32;
                x >= r.x && x < r.right() && y >= r.y && y < r.bottom()
            }
            Annotation::Text { rect, .. } => {
                let r = Rect { x: rect.x - TOL as i32, y: rect.y - TOL as i32, width: rect.width + 2 * TOL as u32, height: rect.height + 2 * TOL as u32 };
                let x = pt.0 as i32;
                let y = pt.1 as i32;
                x >= r.x && x < r.right() && y >= r.y && y < r.bottom()
            }
            Annotation::Arrow { from, to, stroke_width, .. } => {
                let tol = stroke_width.max(1.0) * 0.5 + TOL;
                point_to_segment_dist(pt, *from, *to) <= tol
            }
            Annotation::Brush { points, stroke_width, .. } => {
                if points.len() < 2 {
                    return false;
                }
                let tol = stroke_width.max(1.0) * 0.5 + TOL;
                for w in points.windows(2) {
                    if point_to_segment_dist(pt, w[0], w[1]) <= tol {
                        return true;
                    }
                }
                false
            }
        }
    }

    /// 整体平移（物理像素）。
    pub fn translate(&mut self, dx: f32, dy: f32) {
        let dx_i = dx as i32;
        let dy_i = dy as i32;
        match self {
            Annotation::Rect { rect, .. } | Annotation::Mosaic { rect, .. } => {
                rect.x += dx_i;
                rect.y += dy_i;
            }
            Annotation::Arrow { from, to, .. } => {
                from.0 += dx;
                from.1 += dy;
                to.0 += dx;
                to.1 += dy;
            }
            Annotation::Brush { points, .. } => {
                for p in points.iter_mut() {
                    p.0 += dx;
                    p.1 += dy;
                }
            }
            Annotation::Text { rect, .. } => {
                rect.x += dx_i;
                rect.y += dy_i;
            }
        }
    }
}

/// 点到线段距离（物理像素）。
fn point_to_segment_dist(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let abx = b.0 - a.0;
    let aby = b.1 - a.1;
    let apx = p.0 - a.0;
    let apy = p.1 - a.1;
    let ab2 = abx * abx + aby * aby;
    if ab2 < 1e-6 {
        return (apx * apx + apy * apy).sqrt();
    }
    let t = ((apx * abx + apy * aby) / ab2).clamp(0.0, 1.0);
    let cx = a.0 + t * abx;
    let cy = a.1 + t * aby;
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}

/// 标注管理器：已提交标注（撤销栈）+ 进行中的笔画 + 选中/拖动态。
///
/// 编辑态画布把鼠标手势（按下/拖动/释放）翻译成
/// [`begin_stroke`](Self::begin_stroke) / [`update_stroke`](Self::update_stroke) /
/// [`commit_stroke`](Self::commit_stroke) 调用，标注的具体构建规则集中在这里，
/// UI 层不关心各工具的手势差异。
///
/// 选中/拖动（无工具默认态左键整体移动）：`selected` 指向被选中标注下标，
/// `drag` 记录本次拖动的起点与原始快照，`CursorMoved` 期间实时改写当前序列
/// 但仅在 `commit_drag` 时压一次可撤销历史（`update_drag` 内为临时可变修改，
/// `cancel_drag` 回滚）。
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
    /// 当前遮挡样式（马赛克工具新建标注使用）。
    pub mosaic_style: MosaicStyle,
    /// 当前文字字号（物理像素，新建/编辑文字使用）。
    pub text_font_size: f32,
    /// 当前文字是否加粗。
    pub text_bold: bool,
    /// 当前选中标注下标（无选中为 None）。
    selected: Option<usize>,
    /// 拖动态（按下命中后进入）。
    drag: Option<DragState>,
    /// 自增 id，下一次新建标注分配。
    next_id: u64,
}

/// 单次拖动的临时状态。
#[derive(Debug, Clone)]
struct DragState {
    index: usize,
    /// 按下时的光标位置（物理像素）。
    start: (f32, f32),
    /// 拖动前该标注的原始快照（用于增量平移与取消回滚）。
    origin: Annotation,
    /// 拖动起点对应的历史快照长度（用于判断是否已压历史）。
    _history_len: usize,
}

impl Default for AnnotationManager {
    fn default() -> Self {
        Self {
            stack: UndoStack::new(),
            in_progress: None,
            stroke_anchor: (0.0, 0.0),
            stroke_color: Color::RED,
            stroke_width: 3.0,
            mosaic_style: MosaicStyle::default(),
            text_font_size: 20.0,
            text_bold: false,
            selected: None,
            drag: None,
            next_id: 1,
        }
    }
}

impl AnnotationManager {
    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }
}

impl AnnotationManager {
    /// 开始一笔笔画（按下鼠标）。
    ///
    /// 文字工具不走拖动手势（点击放置 + 文本输入，随文字工具任务落地），
    /// 这里不产生进行中标注。
    pub fn begin_stroke(&mut self, tool: Tool, at: (f32, f32)) {
        self.stroke_anchor = at;
        let id = self.alloc_id();
        self.in_progress = match tool {
            Tool::Rect => Some(Annotation::Rect {
                id,
                rect: Rect::from_points(at.0 as i32, at.1 as i32, at.0 as i32, at.1 as i32),
                color: self.stroke_color,
                stroke_width: self.stroke_width,
            }),
            Tool::Arrow => Some(Annotation::Arrow {
                id,
                from: at,
                to: at,
                color: self.stroke_color,
                stroke_width: self.stroke_width,
            }),
            Tool::Brush => Some(Annotation::Brush {
                id,
                points: vec![at],
                color: self.stroke_color,
                stroke_width: self.stroke_width,
                highlighter: false,
            }),
            Tool::Mosaic => Some(Annotation::Mosaic {
                id,
                rect: Rect::from_points(at.0 as i32, at.1 as i32, at.0 as i32, at.1 as i32),
                style: self.mosaic_style.clone(),
            }),
            Tool::Text => Some(Annotation::Text {
                id,
                rect: Rect::from_points(at.0 as i32, at.1 as i32, at.0 as i32, at.1 as i32),
                content: String::from("文本"),
                color: self.stroke_color,
                font_size: self.text_font_size,
                bold: self.text_bold,
            }),
        };
    }

    /// 更新进行中的笔画（拖动中，物理像素坐标）。
    pub fn update_stroke(&mut self, at: (f32, f32)) {
        let anchor = self.stroke_anchor;
        match &mut self.in_progress {
            Some(Annotation::Rect { rect, .. })
            | Some(Annotation::Mosaic { rect, .. })
            | Some(Annotation::Text { rect, .. }) => {
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

    /// 替换进行中标注（用于选区钳制时不走历史）。
    pub fn replace_in_progress(&mut self, ann: Annotation) {
        self.in_progress = Some(ann);
    }

    /// 设置当前遮挡样式（仅影响后续新建标注，不实时改写已有标注）。
    pub fn set_mosaic_style(&mut self, style: MosaicStyle) {
        self.mosaic_style = style;
    }

    /// 设置当前文字字号（仅影响后续新建/编辑后提交的文字）。
    pub fn set_text_font_size(&mut self, size: f32) {
        self.text_font_size = size.clamp(8.0, 120.0);
    }

    /// 设置当前文字是否加粗。
    pub fn set_text_bold(&mut self, bold: bool) {
        self.text_bold = bold;
    }

    /// 直接提交一条文字标注（点击输入确认后调用，绕开 in_progress）。
    pub fn push_text(&mut self, rect: Rect, content: String) {
        if content.trim().is_empty() {
            return;
        }
        // 保证文本框有最小可编辑尺寸
        let rect = if rect.width < 24 || rect.height < 16 {
            Rect { x: rect.x, y: rect.y, width: rect.width.max(80), height: rect.height.max(28) }
        } else {
            rect
        };
        let ann = Annotation::Text {
            id: self.alloc_id(),
            rect,
            content,
            color: self.stroke_color,
            font_size: self.text_font_size,
            bold: self.text_bold,
        };
        self.stack.push(ann);
        self.selected = Some(self.stack.annotations().len() - 1);
    }

    /// 便捷：在点位创建默认大小文本框。
    pub fn push_text_at(&mut self, pos: (f32, f32), content: String) {
        let rect = Rect { x: pos.0 as i32, y: pos.1 as i32, width: 200, height: 40 };
        self.push_text(rect, content);
    }

    /// 更新已提交的文字标注内容/样式（双击编辑后确认调用）。
    pub fn update_text(&mut self, index: usize, content: String, color: Color, font_size: f32, bold: bool) -> bool {
        if content.trim().is_empty() {
            return false;
        }
        if index >= self.stack.annotations().len() {
            return false;
        }
        // 非文字标注不改
        if !matches!(self.stack.annotations()[index], Annotation::Text { .. }) {
            return false;
        }
        self.stack.edit_current(|vec| {
            if let Some(Annotation::Text { content: c, color: col, font_size: fs, bold: b, .. }) = vec.get_mut(index) {
                *c = content.clone();
                *col = color;
                *fs = font_size;
                *b = bold;
                true
            } else {
                false
            }
        })
    }

    /// 更新文字框几何（拖动缩放句柄时调用）。
    pub fn update_text_rect(&mut self, index: usize, new_rect: Rect) -> bool {
        if index >= self.stack.annotations().len() {
            return false;
        }
        if !matches!(self.stack.annotations()[index], Annotation::Text { .. }) {
            return false;
        }
        self.stack.edit_current(|vec| {
            if let Some(Annotation::Text { rect, .. }) = vec.get_mut(index) {
                *rect = new_rect;
                true
            } else {
                false
            }
        })
    }

    /// 仅更新文字颜色（选中态实时预览用）。
    pub fn set_text_color_at(&mut self, index: usize, color: Color) -> bool {
        if index >= self.stack.annotations().len() { return false; }
        if !matches!(self.stack.annotations()[index], Annotation::Text { .. }) { return false; }
        self.stack.edit_current(|vec| {
            if let Some(Annotation::Text { color: c, .. }) = vec.get_mut(index) { *c = color; true } else { false }
        })
    }
    /// 仅更新文字字号。
    pub fn set_text_font_size_at(&mut self, index: usize, size: f32) -> bool {
        if index >= self.stack.annotations().len() { return false; }
        if !matches!(self.stack.annotations()[index], Annotation::Text { .. }) { return false; }
        let size = size.clamp(8.0, 120.0);
        self.stack.edit_current(|vec| {
            if let Some(Annotation::Text { font_size: fs, .. }) = vec.get_mut(index) { *fs = size; true } else { false }
        })
    }
    /// 仅更新文字加粗。
    pub fn set_text_bold_at(&mut self, index: usize, bold: bool) -> bool {
        if index >= self.stack.annotations().len() { return false; }
        if !matches!(self.stack.annotations()[index], Annotation::Text { .. }) { return false; }
        self.stack.edit_current(|vec| {
            if let Some(Annotation::Text { bold: b, .. }) = vec.get_mut(index) { *b = bold; true } else { false }
        })
    }

    // ── 选中 / 命中 / 拖动（无工具默认态整体移动）─────────────────────

    /// 当前选中下标。
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// 是否正在拖动标注。
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// 命中测试（返回最上层命中标注的下标，顶层为绘制顺序末尾）。
    pub fn hit_test(&self, pt: (f32, f32)) -> Option<usize> {
        for (i, a) in self.stack.annotations().iter().enumerate().rev() {
            if a.hit_test(pt) {
                return Some(i);
            }
        }
        None
    }

    /// 选中指定下标（越界则清空选中）。
    pub fn select(&mut self, index: Option<usize>) {
        if let Some(i) = index {
            if i < self.stack.annotations().len() {
                self.selected = Some(i);
                return;
            }
        }
        self.selected = None;
    }

    /// 在给定点尝试开始拖动：命中则选中并进入拖动态，返回是否命中。
    pub fn begin_drag(&mut self, at: (f32, f32)) -> bool {
        if let Some(i) = self.hit_test(at) {
            let origin = self.stack.annotations()[i].clone();
            self.selected = Some(i);
            self.drag = Some(DragState { index: i, start: at, origin, _history_len: 0 });
            return true;
        }
        self.selected = None;
        false
    }

    /// 拖动中更新（实时改写当前序列对应标注的位置，不压历史）。
    pub fn update_drag(&mut self, at: (f32, f32)) {
        if let Some(d) = self.drag.clone() {
            let dx = at.0 - d.start.0;
            let dy = at.1 - d.start.1;
            if let Some(cur) = self.stack.annotations_mut().get_mut(d.index) {
                *cur = d.origin.clone();
                cur.translate(dx, dy);
            }
        }
    }

    /// 提交拖动（压入一次可撤销历史）。未在拖动中返回 false。
    pub fn commit_drag(&mut self) -> bool {
        if let Some(d) = self.drag.take() {
            // 当前序列已在 update_drag 中就位（history 顶已被临时改写为拖后态），
            // 需先恢复为拖前态，再以拖后态为新快照压栈，否则原态丢失导致撤销失效
            let mutated = self.stack.annotations().to_vec();
            let origin_vec = {
                let mut v = mutated.clone();
                if d.index < v.len() {
                    v[d.index] = d.origin.clone();
                }
                v
            };
            if mutated == origin_vec {
                // 零位移：回滚临时改写
                if let Some(cur) = self.stack.annotations_mut().get_mut(d.index) {
                    *cur = d.origin.clone();
                }
                return false;
            }
            // 回滚顶快照到原态，再压入新快照 = [原态, 新态]
            if let Some(cur) = self.stack.annotations_mut().get_mut(d.index) {
                *cur = d.origin.clone();
            }
            self.stack.edit_current(|next| {
                *next = mutated.clone();
                true
            });
            self.selected = Some(d.index);
            return true;
        }
        false
    }

    /// 取消拖动（回滚到起点）。
    pub fn cancel_drag(&mut self) {
        if let Some(d) = self.drag.take() {
            if let Some(cur) = self.stack.annotations_mut().get_mut(d.index) {
                *cur = d.origin.clone();
            }
            self.selected = Some(d.index);
        }
    }

    /// 撤销最近一次标注。
    pub fn undo(&mut self) -> bool {
        let ok = self.stack.undo();
        if ok {
            // 撤销后选中失效（避免悬空下标）
            if let Some(sel) = self.selected {
                if sel >= self.stack.annotations().len() {
                    self.selected = None;
                }
            }
            self.drag = None;
        }
        ok
    }

    /// 重做最近一次撤销。
    pub fn redo(&mut self) -> bool {
        let ok = self.stack.redo();
        if ok {
            self.drag = None;
        }
        ok
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

    /// 可变访问已提交标注（拖动/缩放等原地编辑用，调用方需自行保证历史记录）。
    pub fn annotations_mut(&mut self) -> &mut Vec<Annotation> {
        self.stack.annotations_mut()
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
/// 各工具的光栅化实现随 Phase 3 逐个落地于 `tools/`；
/// 未落地的工具记录警告并跳过（不影响其他标注写入）。
pub fn apply_to_image(img: &mut image::RgbaImage, annotations: &[Annotation], origin: (i32, i32)) {
    for ann in annotations {
        match ann {
            Annotation::Rect { rect, color, stroke_width, .. } => {
                let local = Rect {
                    x: rect.x - origin.0,
                    y: rect.y - origin.1,
                    width: rect.width,
                    height: rect.height,
                };
                tools::rect::draw_rect(img, local, *color, *stroke_width);
            }
            Annotation::Arrow { from, to, color, stroke_width, .. } => {
                let local_from = (from.0 - origin.0 as f32, from.1 - origin.1 as f32);
                let local_to = (to.0 - origin.0 as f32, to.1 - origin.1 as f32);
                tools::arrow::draw_arrow(img, local_from, local_to, *color, *stroke_width);
            }
            Annotation::Brush { points, color, stroke_width, highlighter, .. } => {
                let local_pts: Vec<(f32, f32)> = points.iter().map(|&(x, y)| (x - origin.0 as f32, y - origin.1 as f32)).collect();
                tools::brush::draw_brush(img, &local_pts, *color, *stroke_width, *highlighter);
            }
            Annotation::Mosaic { rect, style, .. } => {
                let local = Rect { x: rect.x - origin.0, y: rect.y - origin.1, width: rect.width, height: rect.height };
                match style {
                    MosaicStyle::Pixelate { block_size } => tools::mosaic::draw_pixelate(img, local, *block_size),
                    MosaicStyle::Blur { radius } => tools::mosaic::draw_blur(img, local, *radius),
                    MosaicStyle::Solid { color } => tools::mosaic::draw_solid(img, local, *color),
                }
            }
            Annotation::Text { rect, content, color, font_size, bold, .. } => {
                let local_rect = Rect { x: rect.x - origin.0, y: rect.y - origin.1, width: rect.width, height: rect.height };
                tools::text::draw_text_in_rect(img, local_rect, content, *color, *font_size, *bold);
            }
        }
    }
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
        assert!(mgr.in_progress().is_some());
        mgr.commit_stroke();
        assert_eq!(mgr.annotations().len(), 1);
        match &mgr.annotations()[0] {
            Annotation::Text { content, .. } => assert_eq!(content, "文本"),
            other => panic!("应为文字标注: {other:?}"),
        }
    }

    #[test]
    fn annotation_degeneracy_rules() {
        assert!(Annotation::Arrow { id: 1, from: (0.0, 0.0), to: (1.0, 0.0), color: Color::RED, stroke_width: 2.0 }.is_degenerate());
        assert!(!Annotation::Arrow { id: 2, from: (0.0, 0.0), to: (10.0, 0.0), color: Color::RED, stroke_width: 2.0 }.is_degenerate());
        assert!(Annotation::Text { id: 1, rect: Rect { x: 0, y: 0, width: 100, height: 30 }, content: "  ".into(), color: Color::RED, font_size: 16.0, bold: false }.is_degenerate());
    }

    #[test]
    fn apply_to_image_translates_by_selection_origin() {
        use crate::utils::math::Rect;
        let mut img = image::RgbaImage::from_pixel(50, 50, image::Rgba([255, 255, 255, 255]));
        // 全图坐标 (20, 30) 的矩形，选区原点 (10, 20) → 图内应落在 (10, 10)
        let ann = Annotation::Rect {
            id: 1,
            rect: Rect { x: 20, y: 30, width: 15, height: 10 },
            color: Color::RED,
            stroke_width: 1.0,
        };
        apply_to_image(&mut img, &[ann], (10, 20));
        let at = |x: u32, y: u32| {
            let p = img.get_pixel(x, y).0;
            [p[0], p[1], p[2]]
        };
        // 平移后描边贴矩形外侧：上条带 y ∈ [9,10)、下条带 y ∈ [20,21)
        assert_eq!(at(12, 9), [255, 59, 48]);
        assert_eq!(at(25, 20), [255, 59, 48]);
        // 内部保持白
        assert_eq!(at(15, 15), [255, 255, 255]);
    }

    #[test]
    fn arrow_translate_and_hit() {
        let mut ann = Annotation::Arrow { id: 1, from: (10.0, 10.0), to: (30.0, 10.0), color: Color::RED, stroke_width: 2.0 };
        assert!(ann.hit_test((20.0, 10.0)));
        assert!(!ann.hit_test((20.0, 30.0)));
        ann.translate(5.0, 5.0);
        match ann {
            Annotation::Arrow { from, to, .. } => {
                assert_eq!(from, (15.0, 15.0));
                assert_eq!(to, (35.0, 15.0));
            }
            _ => panic!("箭头"),
        }
    }

    #[test]
    fn rect_hit_and_translate() {
        let mut ann = Annotation::Rect { id: 1, rect: Rect { x: 10, y: 10, width: 20, height: 10 }, color: Color::RED, stroke_width: 2.0 };
        assert!(ann.hit_test((15.0, 15.0))); // 内部命中（拖动友好）
        assert!(!ann.hit_test((0.0, 0.0)));
        ann.translate(3.0, -2.0);
        match ann {
            Annotation::Rect { rect, .. } => assert_eq!(rect, Rect { x: 13, y: 8, width: 20, height: 10 }),
            _ => panic!("矩形"),
        }
    }

    #[test]
    fn annotation_manager_drag_commit_and_undo() {
        let mut mgr = AnnotationManager::default();
        mgr.begin_stroke(Tool::Rect, (10.0, 10.0));
        mgr.update_stroke((30.0, 20.0));
        mgr.commit_stroke();
        assert_eq!(mgr.annotations().len(), 1);
        // 命中并拖动
        assert!(mgr.begin_drag((15.0, 15.0)));
        mgr.update_drag((20.0, 20.0));
        mgr.commit_drag();
        match &mgr.annotations()[0] {
            Annotation::Rect { rect, .. } => assert_eq!(rect, &Rect { x: 15, y: 15, width: 20, height: 10 }),
            _ => panic!("矩形"),
        }
        // 撤销拖动
        assert!(mgr.undo());
        match &mgr.annotations()[0] {
            Annotation::Rect { rect, .. } => assert_eq!(rect, &Rect { x: 10, y: 10, width: 20, height: 10 }),
            _ => panic!("矩形"),
        }
        assert!(mgr.redo());
    }

    #[test]
    fn hit_test_returns_topmost() {
        let mut mgr = AnnotationManager::default();
        mgr.begin_stroke(Tool::Rect, (0.0, 0.0));
        mgr.update_stroke((20.0, 20.0));
        mgr.commit_stroke();
        mgr.begin_stroke(Tool::Rect, (5.0, 5.0));
        mgr.update_stroke((30.0, 30.0));
        mgr.commit_stroke();
        // (10,10) 命中两矩形，应返回后者（索引 1）
        assert_eq!(mgr.hit_test((10.0, 10.0)), Some(1));
    }

    #[test]
    fn apply_to_image_arrow_translates() {
        let mut img = image::RgbaImage::from_pixel(50, 50, image::Rgba([255, 255, 255, 255]));
        let ann = Annotation::Arrow { id: 1, from: (20.0, 25.0), to: (40.0, 25.0), color: Color::RED, stroke_width: 2.0 };
        apply_to_image(&mut img, &[ann], (10, 20));
        // 平移后箭头 (10,5)->(30,5) 轴线在 y=5 附近
        let p = img.get_pixel(20, 5).0;
        assert_eq!(p[0..3], [255, 59, 48]);
    }
}
