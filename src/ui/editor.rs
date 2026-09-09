//! 标注编辑器模块（仅 Windows 平台编译）。

use std::collections::HashMap;

use crate::annotation::{Annotation, AnnotationManager, Color, Tool};
use crate::utils::math::Rect;
use libblur::{stack_blur, FastBlurChannels, ThreadingPolicy};

/// 文字编辑态（矩形文本框，新建/二次编辑）。
#[derive(Debug, Clone)]
pub struct TextEditState {
    /// 文本框（物理像素）。
    pub rect: Rect,
    /// 输入缓冲（多行，以 `\n` 分隔）。
    pub buffer: String,
    /// 正在编辑的已提交标注下标（`None` 为新建）。
    pub index: Option<usize>,
}

/// 文本框缩放状态。
#[derive(Debug, Clone)]
struct TextResizeState {
    index: usize,
    handle: usize, // 0 TL, 1 TR, 2 BR, 3 BL
    start_pt: (f32, f32),
    start_rect: Rect,
}

struct BlurCacheEntry {
    padded_rect: Rect,
    radius: f32,
    blurred: image::RgbaImage,
    /// 构建时已提交标注的修订号（标注变更即重建，保证"下层标注被覆盖效果"与导出一致）。
    rev: u64,
    handle: egui::TextureHandle,
}

/// 标注编辑器状态。
pub struct Editor {
    mgr: AnnotationManager,
    active_tool: Option<Tool>,
    editing_text: Option<TextEditState>,
    resizing_text: Option<TextResizeState>,
    blur_cache: HashMap<u64, BlurCacheEntry>,
    /// 在途预览（in_progress）专用缓存，避免每帧新建纹理。
    preview_blur: Option<BlurCacheEntry>,
    /// 字体悬停预览态（选中文字 idx + 原字体，弹层悬停实时换字体用）。
    font_hover: Option<(usize, Option<String>)>,
    /// 选区边界（物理像素），标注创建/拖动/缩放均钳制于此（防止拖出选区外并遮挡工具条）。
    selection: Option<Rect>,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl Editor {
    pub fn new() -> Self {
        Self { mgr: AnnotationManager::default(), active_tool: None, editing_text: None, resizing_text: None, blur_cache: HashMap::new(), preview_blur: None, selection: None, font_hover: None }
    }

    /// 设置选区边界（`Overlay` 每帧同步），`None` 表示无限制。
    pub fn set_selection(&mut self, sel: Option<Rect>) {
        self.selection = sel;
    }

    fn clamp_pt(&self, pt: (f32, f32)) -> (f32, f32) {
        if let Some(b) = self.selection {
            let x = (pt.0 as i32).clamp(b.x, b.right() - 1) as f32;
            let y = (pt.1 as i32).clamp(b.y, b.bottom() - 1) as f32;
            (x, y)
        } else {
            pt
        }
    }

    fn clamp_rect(&self, r: Rect) -> Rect {
        if let Some(b) = self.selection {
            // 保持尺寸，位置钳制；若本身大于选区则收缩到选区
            r.clamp(&b)
        } else {
            r
        }
    }

    /// 清理不存在于当前标注列表的 blur 缓存（undo/redo/delete 后）。
    fn prune_blur_cache(&mut self) {
        let live: std::collections::HashSet<u64> = self.mgr.annotations().iter().map(|a| a.id()).collect();
        self.blur_cache.retain(|k, _| live.contains(k));
        // preview_blur 的 id 若已提交则会转正，无需额外清理，下次重算即可
    }
    pub fn active_tool(&self) -> Option<Tool> { self.active_tool }
    pub fn is_stroking(&self) -> bool { self.mgr.in_progress().is_some() }
    pub fn is_dragging(&self) -> bool { self.mgr.is_dragging() }
    pub fn is_resizing_text(&self) -> bool { self.resizing_text.is_some() }
    pub fn selected(&self) -> Option<usize> { self.mgr.selected() }
    pub fn stroke_color(&self) -> Color { self.mgr.stroke_color }
    pub fn set_stroke_color(&mut self, c: Color) {
        self.mgr.stroke_color = c;
        // 选中文字实时跟色（非编辑态）
        if self.editing_text.is_none() {
            if let Some(idx) = self.mgr.selected() {
                if matches!(self.mgr.annotations().get(idx), Some(Annotation::Text{..})) {
                    let _ = self.mgr.set_text_color_at(idx, c);
                }
            }
        }
    }
    pub fn stroke_width(&self) -> f32 { self.mgr.stroke_width }
    pub fn set_stroke_width(&mut self, w: f32) { self.mgr.stroke_width = w; }
    pub fn text_font_size(&self) -> f32 { self.mgr.text_font_size }
    pub fn set_text_font_size(&mut self, s: f32) {
        self.mgr.set_text_font_size(s);
        if self.editing_text.is_none() {
            if let Some(idx) = self.mgr.selected() {
                if matches!(self.mgr.annotations().get(idx), Some(Annotation::Text{..})) {
                    let _ = self.mgr.set_text_font_size_at(idx, s);
                }
            }
        }
    }
    pub fn text_bold(&self) -> bool { self.mgr.text_bold }
    pub fn set_text_bold(&mut self, b: bool) {
        self.mgr.set_text_bold(b);
        if self.editing_text.is_none() {
            if let Some(idx) = self.mgr.selected() {
                if matches!(self.mgr.annotations().get(idx), Some(Annotation::Text{..})) {
                    let _ = self.mgr.set_text_bold_at(idx, b);
                }
            }
        }
    }
    pub fn mosaic_style(&self) -> crate::annotation::MosaicStyle { self.mgr.mosaic_style.clone() }
    pub fn set_mosaic_style(&mut self, s: crate::annotation::MosaicStyle) { self.mgr.set_mosaic_style(s); }
    pub fn activate(&mut self, tool: Tool) {
        if self.editing_text.is_some() && tool != Tool::Text { self.commit_text_edit(); }
        self.active_tool = Some(tool);
    }
    pub fn deactivate(&mut self) {
        self.active_tool = None;
        self.mgr.cancel_stroke();
        self.cancel_text_edit();
        self.resizing_text = None;
    }

    // ── 文字编辑态 ──
    pub fn is_editing_text(&self) -> bool { self.editing_text.is_some() }
    pub fn text_edit_state(&self) -> Option<&TextEditState> { self.editing_text.as_ref() }
    pub fn text_edit_state_mut(&mut self) -> Option<&mut TextEditState> { self.editing_text.as_mut() }

    pub fn begin_text_edit_with_rect(&mut self, rect: Rect) {
        let rect = if rect.width < 24 || rect.height < 16 { Rect{ x: rect.x, y: rect.y, width: 200, height: 60 } } else { rect };
        self.editing_text = Some(TextEditState{ rect, buffer: String::new(), index: None });
        self.mgr.select(None);
    }
    pub fn begin_text_edit_existing(&mut self, index: usize) -> bool {
        let Some(Annotation::Text{ rect, content, color, font_size, bold, .. }) = self.mgr.annotations().get(index).cloned() else { return false; };
        self.mgr.stroke_color = color;
        self.mgr.text_font_size = font_size;
        self.mgr.text_bold = bold;
        self.editing_text = Some(TextEditState{ rect, buffer: content, index: Some(index) });
        self.mgr.select(Some(index));
        true
    }
    pub fn commit_text_edit(&mut self) -> bool {
        let Some(state) = self.editing_text.take() else { return false; };
        if state.buffer.trim().is_empty() { return false; }
        if let Some(idx) = state.index {
            let ok = self.mgr.update_text(idx, state.buffer, self.mgr.stroke_color, self.mgr.text_font_size, self.mgr.text_bold);
            if ok { self.mgr.select(Some(idx)); }
            // 同步更新几何（编辑期间可能缩放过）
            let _ = self.mgr.update_text_rect(idx, state.rect);
            ok
        } else {
            self.mgr.push_text(state.rect, state.buffer);
            true
        }
    }
    pub fn cancel_text_edit(&mut self) { self.editing_text = None; }

    // ── 文本框缩放 ──
    pub fn hit_text_handle(&self, pt: (f32,f32)) -> Option<(usize, usize)> {
        const HS: f32 = 8.0;
        // 编辑态的未提交文本框也支持缩放（index 用 usize::MAX 标记，调用方需特殊处理）
        if let Some(st) = &self.editing_text {
            let rect = st.rect;
            let x = rect.x as f32; let y = rect.y as f32;
            let rx = rect.right() as f32; let by = rect.bottom() as f32;
            let mx = (x+rx)*0.5; let my = (y+by)*0.5;
            let pts = [(x, y), (mx, y), (rx, y), (rx, my), (rx, by), (mx, by), (x, by), (x, my)];
            for (h, (cx, cy)) in pts.iter().enumerate() {
                if (pt.0 - cx).abs() <= HS && (pt.1 - cy).abs() <= HS {
                    // 未提交时用哨兵 index，调用方直接改 editing rect
                    return Some((usize::MAX, h));
                }
            }
        }
        for (i, ann) in self.mgr.annotations().iter().enumerate().rev() {
            if let Annotation::Text{ rect, .. } = ann {
                let x = rect.x as f32; let y = rect.y as f32;
                let rx = rect.right() as f32; let by = rect.bottom() as f32;
                let mx = (x+rx)*0.5; let my = (y+by)*0.5;
                let pts = [
                    (x, y), (mx, y), (rx, y), (rx, my),
                    (rx, by), (mx, by), (x, by), (x, my),
                ];
                for (h, (cx, cy)) in pts.iter().enumerate() {
                    if (pt.0 - cx).abs() <= HS && (pt.1 - cy).abs() <= HS { return Some((i, h)); }
                }
            }
        }
        None
    }
    pub fn begin_text_resize(&mut self, index: usize, handle: usize, at: (f32,f32)) -> bool {
        if index == usize::MAX {
            if let Some(st) = &self.editing_text {
                self.resizing_text = Some(TextResizeState{ index, handle, start_pt: at, start_rect: st.rect });
                return true;
            }
            return false;
        }
        if let Some(Annotation::Text{ rect, .. }) = self.mgr.annotations().get(index).cloned() {
            self.resizing_text = Some(TextResizeState{ index, handle, start_pt: at, start_rect: rect });
            self.mgr.select(Some(index));
            return true;
        }
        false
    }
    pub fn update_text_resize(&mut self, at: (f32,f32)) {
        let at = self.clamp_pt(at);
        if let Some(rs) = self.resizing_text.clone() {
            let dx = at.0 - rs.start_pt.0;
            let dy = at.1 - rs.start_pt.1;
            let mut r = rs.start_rect;
            match rs.handle {
                0 => { r.x = (r.x as f32 + dx) as i32; r.y = (r.y as f32 + dy) as i32; r.width = (r.width as f32 - dx).max(24.0) as u32; r.height = (r.height as f32 - dy).max(16.0) as u32; }
                1 => { r.y = (r.y as f32 + dy) as i32; r.height = (r.height as f32 - dy).max(16.0) as u32; }
                2 => { r.y = (r.y as f32 + dy) as i32; r.width = (r.width as f32 + dx).max(24.0) as u32; r.height = (r.height as f32 - dy).max(16.0) as u32; }
                3 => { r.width = (r.width as f32 + dx).max(24.0) as u32; }
                4 => { r.width = (r.width as f32 + dx).max(24.0) as u32; r.height = (r.height as f32 + dy).max(16.0) as u32; }
                5 => { r.height = (r.height as f32 + dy).max(16.0) as u32; }
                6 => { r.x = (r.x as f32 + dx) as i32; r.width = (r.width as f32 - dx).max(24.0) as u32; r.height = (r.height as f32 + dy).max(16.0) as u32; }
                7 => { r.x = (r.x as f32 + dx) as i32; r.width = (r.width as f32 - dx).max(24.0) as u32; }
                _ => {}
            }
            r = self.clamp_rect(r);
            if rs.index == usize::MAX {
                if let Some(edit) = &mut self.editing_text { edit.rect = r; }
                return;
            }
            if let Some(edit) = &mut self.editing_text { if edit.index == Some(rs.index) { edit.rect = r; } }
            if let Some(cur) = self.mgr.annotations_mut().get_mut(rs.index) {
                if let Annotation::Text{ rect, .. } = &mut *cur { *rect = r; }
            }
        }
    }
    pub fn commit_text_resize(&mut self) -> bool {
        if let Some(rs) = self.resizing_text.take() {
            if rs.index == usize::MAX { return true; }
            let new_rect = self.mgr.annotations().get(rs.index).and_then(|a| if let Annotation::Text{ rect, .. } = a { Some(*rect) } else { None });
            if let Some(nr) = new_rect { let _ = self.mgr.update_text_rect(rs.index, nr); }
            return true;
        }
        false
    }
    pub fn cancel_text_resize(&mut self) {
        if let Some(rs) = self.resizing_text.take() {
            if rs.index == usize::MAX {
                if let Some(edit) = &mut self.editing_text { edit.rect = rs.start_rect; }
                return;
            }
            if let Some(cur) = self.mgr.annotations_mut().get_mut(rs.index) {
                if let Annotation::Text{ rect, .. } = &mut *cur { *rect = rs.start_rect; }
            }
        }
    }

    // ── 笔画代理 ──
    pub fn begin_stroke(&mut self, at: (f32,f32)) {
        if self.editing_text.is_some() || self.resizing_text.is_some() { return; }
        // 选区外不开始新笔画（避免在遮罩暗区误画并遮挡工具条）
        if let Some(b) = self.selection {
            if (at.0 as i32) < b.x || (at.0 as i32) >= b.right() || (at.1 as i32) < b.y || (at.1 as i32) >= b.bottom() {
                return;
            }
        }
        let at = self.clamp_pt(at);
        if let Some(tool) = self.active_tool {
            if tool == Tool::Text {
                self.mgr.begin_stroke(tool, at);
                return;
            }
            self.mgr.begin_stroke(tool, at);
        }
    }
    pub fn update_stroke(&mut self, at: (f32,f32)) {
        if self.editing_text.is_some() { return; }
        let at = self.clamp_pt(at);
        self.mgr.update_stroke(at);
        // 钳制在途矩形/箭头/文本框到选区内（防止拖出选区外并在导出后丢失）
        if let Some(bounds) = self.selection {
            if let Some(ann) = self.mgr.in_progress().cloned() {
                let clamped = match ann {
                    Annotation::Rect{ id, rect, color, stroke_width } => {
                        let r = self.clamp_rect(rect);
                        if r != rect { Some(Annotation::Rect{ id, rect: r, color, stroke_width }) } else { None }
                    },
                    Annotation::Mosaic{ id, rect, style } => {
                        let r = self.clamp_rect(rect);
                        if r != rect { Some(Annotation::Mosaic{ id, rect: r, style }) } else { None }
                    },
                    Annotation::Text{ id, rect, content, color, font_size, bold, font } => {
                        let r = self.clamp_rect(rect);
                        if r != rect { Some(Annotation::Text{ id, rect: r, content, color, font_size, bold, font }) } else { None }
                    },
                    Annotation::Arrow{ id, from, to, color, stroke_width } => {
                        let t = self.clamp_pt(to);
                        if t != to { Some(Annotation::Arrow{ id, from, to: t, color, stroke_width }) } else { None }
                    },
                    Annotation::Brush{ .. } => {
                        // 画笔逐点已在添加时钳制，此处不额外处理
                        None
                    },
                };
                if let Some(c) = clamped {
                    // 直接替换 in_progress（不经过历史）
                    self.mgr.replace_in_progress(c);
                }
            }
            // 画笔最后一点若出界则拉回
            if let Some(Annotation::Brush{ .. }) = self.mgr.in_progress() {
                if let Some(Annotation::Brush{ id, mut points, color, stroke_width, highlighter }) = self.mgr.in_progress().cloned() {
                    if let Some(last) = points.last_mut() {
                        let clamped = self.clamp_pt(*last);
                        if *last != clamped { *last = clamped; }
                        self.mgr.replace_in_progress(Annotation::Brush{ id, points, color, stroke_width, highlighter });
                    }
                    // 整体钳制：若有历史点越界则整体拉回（极端情况）
                    let _ = bounds; // 已逐点钳制
                }
            }
        }
    }
    pub fn commit_stroke(&mut self) {
        // 文字工具：拖动创建文本框 -> 转为编辑态，不直接 push 占位
        if self.active_tool == Some(Tool::Text) {
            if let Some(Annotation::Text{ rect, .. }) = self.mgr.in_progress().cloned() {
                let r = rect;
                self.mgr.cancel_stroke(); // 丢弃 in_progress 占位
                self.preview_blur = None;
                self.begin_text_edit_with_rect(r);
                return;
            }
        }
        self.mgr.commit_stroke();
        self.preview_blur = None;
        self.prune_blur_cache();
    }
    pub fn cancel_stroke(&mut self) { self.mgr.cancel_stroke(); self.preview_blur = None; }

    // ── 选中/拖动 ──
    pub fn hit_test(&self, at: (f32,f32)) -> Option<usize> { self.mgr.hit_test(at) }
    pub fn begin_drag(&mut self, at: (f32,f32)) -> bool {
        if self.editing_text.is_some() || self.resizing_text.is_some() { return false; }
        // 文本缩放句柄优先于移动
        if let Some((idx, h)) = self.hit_text_handle(at) {
            return self.begin_text_resize(idx, h, at);
        }
        self.mgr.begin_drag(at)
    }
    pub fn update_drag(&mut self, at: (f32,f32)) {
        if self.resizing_text.is_some() { self.update_text_resize(at); return; }
        let at = self.clamp_pt(at);
        self.mgr.update_drag(at);
        // 拖动后钳制到选区内（防止标注整体拖出选区遮挡工具条或丢失）
        if let Some(bounds) = self.selection {
            if let Some(idx) = self.mgr.selected() {
                let needs_clamp = if let Some(ann) = self.mgr.annotations().get(idx) {
                    match ann {
                        Annotation::Rect{ rect, .. } | Annotation::Mosaic{ rect, .. } | Annotation::Text{ rect, .. } => {
                            rect.x < bounds.x || rect.y < bounds.y || rect.right() > bounds.right() || rect.bottom() > bounds.bottom()
                        },
                        Annotation::Arrow{ from, to, .. } => {
                            let in_bounds = |p:(f32,f32)| (p.0 as i32) >= bounds.x && (p.0 as i32) < bounds.right() && (p.1 as i32) >= bounds.y && (p.1 as i32) < bounds.bottom();
                            !in_bounds(*from) || !in_bounds(*to)
                        },
                        Annotation::Brush{ points, .. } => {
                            points.iter().any(|p| (p.0 as i32) < bounds.x || (p.0 as i32) >= bounds.right() || (p.1 as i32) < bounds.y || (p.1 as i32) >= bounds.bottom())
                        },
                    }
                } else { false };
                if needs_clamp {
                    // 回滚本次位移并以钳制后的位移重新应用：计算当前与 origin 的差值并钳制
                    if let Some(ann) = self.mgr.annotations().get(idx).cloned() {
                        let clamped = match ann {
                            Annotation::Rect{ id, rect, color, stroke_width } => { let r = self.clamp_rect(rect); Annotation::Rect{ id, rect: r, color, stroke_width } },
                            Annotation::Mosaic{ id, rect, style } => { let r = self.clamp_rect(rect); Annotation::Mosaic{ id, rect: r, style } },
                            Annotation::Text{ id, rect, content, color, font_size, bold, font } => { let r = self.clamp_rect(rect); Annotation::Text{ id, rect: r, content, color, font_size, bold, font } },
                            Annotation::Arrow{ id, from, to, color, stroke_width } => {
                                let f = self.clamp_pt(from);
                                let t = self.clamp_pt(to);
                                Annotation::Arrow{ id, from: f, to: t, color, stroke_width }
                            },
                            Annotation::Brush{ id, points, color, stroke_width, highlighter } => {
                                let pts: Vec<(f32,f32)> = points.into_iter().map(|p| self.clamp_pt(p)).collect();
                                Annotation::Brush{ id, points: pts, color, stroke_width, highlighter }
                            },
                        };
                        if let Some(cur) = self.mgr.annotations_mut().get_mut(idx) { *cur = clamped; }
                    }
                }
            }
        }
    }
    pub fn commit_drag(&mut self) -> bool {
        if self.resizing_text.is_some() { return self.commit_text_resize(); }
        self.mgr.commit_drag()
    }
    pub fn cancel_drag(&mut self) {
        if self.resizing_text.is_some() { self.cancel_text_resize(); } else { self.mgr.cancel_drag(); }
    }
    pub fn select(&mut self, idx: Option<usize>) { self.mgr.select(idx); }
    pub fn undo(&mut self) -> bool {
        if self.editing_text.is_some() { self.cancel_text_edit(); return true; }
        if self.resizing_text.is_some() { self.cancel_text_resize(); return true; }
        let ok = self.mgr.undo();
        if ok { self.prune_blur_cache(); }
        ok
    }
    pub fn redo(&mut self) -> bool {
        if self.editing_text.is_some() || self.resizing_text.is_some() { return false; }
        let ok = self.mgr.redo();
        if ok { self.prune_blur_cache(); }
        ok
    }
    /// 字体操作的目标文字下标（编辑态优先，其次选中态）。
    fn font_target_index(&self) -> Option<usize> {
        if let Some(st) = &self.editing_text {
            return st.index;
        }
        self.mgr.selected()
    }

    /// 选中/编辑中文字的独立字体（目标不是文字或无目标返回 `None`）。
    pub fn selected_text_font(&self) -> Option<String> {
        self.font_target_index().and_then(|i| self.mgr.text_font_at(i))
    }

    /// 内联编辑中文字的独立字体（新文字为 None；内联 TextEdit 按此 family 渲染）。
    pub fn editing_text_font(&self) -> Option<String> {
        let idx = self.editing_text.as_ref().and_then(|s| s.index)?;
        match self.mgr.annotations().get(idx) {
            Some(Annotation::Text { font, .. }) => font.clone(),
            _ => None,
        }
    }

    /// 当前是否有可改字体的文字标注（工具条字体选择器可用性）。
    pub fn has_selected_text(&self) -> bool {
        self.font_target_index()
            .is_some_and(|i| matches!(self.mgr.annotations().get(i), Some(Annotation::Text{..})))
    }

    /// 选中/编辑中文字的当前样式（工具条字号/加粗/颜色行实时联动用；
    /// `None` = 无选中文字，工具条走"新标注默认值"路径）。
    pub fn selected_text_style(&self) -> Option<(f32, bool)> {
        let idx = self.font_target_index()?;
        match self.mgr.annotations().get(idx) {
            Some(Annotation::Text { font_size, bold, .. }) => Some((*font_size, *bold)),
            _ => None,
        }
    }

    /// 选中文字实时改字号（走历史可撤销；无选中文字则改"新标注默认值"）。
    ///
    /// 注意默认值与标注都要写：内联编辑预览读 `mgr.text_font_size`（默认值），
    /// 标注预览/导出读标注自身字段——只写一边另一边就"没反应"
    /// （2026-09-10 实机反馈根因）。
    pub fn apply_text_font_size(&mut self, size: f32) {
        self.mgr.set_text_font_size(size);
        if let Some(i) = self.font_target_index() {
            self.mgr.set_text_font_size_at(i, size);
        }
    }

    /// 选中文字实时改加粗（默认值与标注双写，理由同上）。
    pub fn apply_text_bold(&mut self, bold: bool) {
        self.mgr.set_text_bold(bold);
        if let Some(i) = self.font_target_index() {
            self.mgr.set_text_bold_at(i, bold);
        }
    }

    /// 选中文字实时改颜色（默认值与标注双写，理由同上）。
    pub fn apply_text_color(&mut self, color: Color) {
        self.set_stroke_color(color);
        if let Some(i) = self.font_target_index() {
            self.mgr.set_text_color_at(i, color);
        }
    }

    /// 全部在用文字字体（覆盖层每帧渲染前预注册 egui family 用，
    /// 防止 mid-frame set_fonts 不生效导致 FontFamily unbound panic）。
    pub fn all_text_fonts(&self) -> Vec<String> {
        let mut out = Vec::new();
        for a in self.mgr.annotations().iter().chain(self.mgr.in_progress()) {
            if let Annotation::Text { font: Some(f), .. } = a {
                if !out.contains(f) {
                    out.push(f.clone());
                }
            }
        }
        out
    }

    /// 字体悬停实时预览（Word 式：弹层悬停某字体，选中文字立即变体）。
    pub fn hover_text_font(&mut self, font_path: Option<String>) {
        let Some(idx) = self.font_target_index() else { return };
        if !matches!(self.mgr.annotations().get(idx), Some(Annotation::Text{..})) { return; }
        if self.font_hover.as_ref().map(|(i, _)| *i) != Some(idx) {
            self.font_hover = Some((idx, self.mgr.text_font_at(idx)));
        }
        if self.mgr.text_font_at(idx) != font_path {
            self.mgr.set_text_font_raw(idx, font_path);
        }
    }

    /// 结束字体悬停预览：`commit = Some(f)` 提交该字体（走历史可撤销），
    /// `None` 仅还原。
    pub fn end_text_font_hover(&mut self, commit: Option<Option<String>>) {
        if let Some((idx, orig)) = self.font_hover.take() {
            if self.mgr.text_font_at(idx) != orig {
                self.mgr.set_text_font_raw(idx, orig.clone());
            }
            if let Some(f) = commit {
                if f != orig {
                    self.mgr.set_text_font_at(idx, f);
                }
            }
        }
    }

    pub fn can_undo(&self) -> bool { self.mgr.can_undo() }
    pub fn can_redo(&self) -> bool { self.mgr.can_redo() }
    pub fn annotations(&self) -> &[Annotation] { self.mgr.annotations() }

    pub fn draw_annotations(&mut self, painter: &egui::Painter, ctx: &egui::Context, ppp: f32, image: Option<&image::RgbaImage>) {
        // 选区裁剪：所有标注绘制均限制在选区内，防止拖出选区外遮挡工具条或在导出时丢失（导出仅裁剪选区内像素）
        let painter = if let Some(sel) = self.selection {
            let clip = egui::Rect::from_min_max(
                egui::pos2(sel.x as f32 / ppp, sel.y as f32 / ppp),
                egui::pos2(sel.right() as f32 / ppp, sel.bottom() as f32 / ppp),
            );
            painter.with_clip_rect(clip)
        } else {
            painter.clone()
        };
        let painter = &painter;
        let editing_idx = self.editing_text.as_ref().and_then(|s| s.index);
        for (idx, ann) in self.mgr.annotations().iter().enumerate() {
            if Some(idx) == editing_idx { continue; }
            let selected = self.mgr.selected() == Some(idx);
            if let Annotation::Mosaic{ id, rect, style: crate::annotation::MosaicStyle::Blur{ radius } } = ann {
                if let Some(img) = image {
                    // 退化矩形直接占位，避免零尺寸纹理触发 wgpu Validation Error (Dimension X is zero)
                    if rect.width < 2 || rect.height < 2 {
                        let r = egui::Rect::from_min_max(egui::pos2(rect.x as f32/ppp, rect.y as f32/ppp), egui::pos2(rect.right() as f32/ppp, rect.bottom() as f32/ppp));
                        painter.rect_filled(r, 0.0, egui::Color32::from_rgba_unmultiplied(70, 70, 70, 230));
                        painter.text(r.center(), egui::Align2::CENTER_CENTER, "模糊", egui::FontId::proportional(12.0 / ppp.max(1.0)), egui::Color32::WHITE);
                        painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)), egui::StrokeKind::Outside);
                        continue;
                    }
                    let r = egui::Rect::from_min_max(egui::pos2(rect.x as f32/ppp, rect.y as f32/ppp), egui::pos2(rect.right() as f32/ppp, rect.bottom() as f32/ppp));
                    let radius = radius.max(1.0);
                    let blur_id = *id;
                    let cur_rev = self.mgr.rev();
                    let mut done = false;
                    if let Some(entry) = self.blur_cache.get_mut(&blur_id) {
                        if entry.rev == cur_rev && (entry.radius - radius).abs() < 0.01 && rect.x >= entry.padded_rect.x && rect.y >= entry.padded_rect.y && rect.right() <= entry.padded_rect.right() && rect.bottom() <= entry.padded_rect.bottom() {
                            let dx = (rect.x - entry.padded_rect.x) as u32;
                            let dy = (rect.y - entry.padded_rect.y) as u32;
                            let w = rect.width; let h = rect.height;
                            let cropped = image::imageops::crop_imm(&entry.blurred, dx, dy, w, h).to_image();
                            let color_image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], cropped.as_raw());
                            entry.handle.set(color_image, egui::TextureOptions::LINEAR);
                            painter.image(entry.handle.id(), r, egui::Rect::from_min_max(egui::pos2(0.0,0.0), egui::pos2(1.0,1.0)), egui::Color32::WHITE);
                            painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)), egui::StrokeKind::Outside);
                            if selected {
                                let b = ann.bounds();
                                let br = egui::Rect::from_min_max(egui::pos2(b.x as f32/ppp, b.y as f32/ppp), egui::pos2(b.right() as f32/ppp, b.bottom() as f32/ppp));
                                painter.rect_stroke(br, 0.0, egui::Stroke::new(1.0/ppp.max(1.0), egui::Color32::from_rgba_unmultiplied(10,132,255,180)), egui::StrokeKind::Outside);
                            }
                            done = true;
                        }
                    }
                    if done { continue; }
                    // 未命中/半径变化/标注变更：重算 padded 模糊（方案B stack_blur O(1)）。
                    // 先把本马赛克之前的标注画进裁剪块再模糊——与导出 apply_to_image 的
                    // 顺序一致，保证"被马赛克覆盖的下层标注"预览与导出像素相同（2026-09-09）。
                    let padded = padded_rect_for_blur(*rect, img);
                    if padded.width >= 2 && padded.height >= 2 {
                        let x0 = padded.x as u32; let y0 = padded.y as u32;
                        let pw = padded.width; let ph = padded.height;
                        let mut patch = image::imageops::crop_imm(img, x0, y0, pw, ph).to_image();
                        crate::annotation::apply_to_image(&mut patch, &self.mgr.annotations()[..idx], (padded.x, padded.y));
                        stack_blur_rgba(&mut patch, radius as u32);
                        // 缓存整块 padded 模糊
                        let entry_exists = self.blur_cache.contains_key(&blur_id);
                        let dx = (rect.x - padded.x) as u32; let dy = (rect.y - padded.y) as u32;
                        let cropped = image::imageops::crop_imm(&patch, dx, dy, rect.width, rect.height).to_image();
                        let color_image = egui::ColorImage::from_rgba_unmultiplied([rect.width as usize, rect.height as usize], cropped.as_raw());
                        if entry_exists {
                            if let Some(entry) = self.blur_cache.get_mut(&blur_id) {
                                entry.padded_rect = padded;
                                entry.radius = radius;
                                entry.blurred = patch;
                                entry.rev = cur_rev;
                                entry.handle.set(color_image, egui::TextureOptions::LINEAR);
                                painter.image(entry.handle.id(), r, egui::Rect::from_min_max(egui::pos2(0.0,0.0), egui::pos2(1.0,1.0)), egui::Color32::WHITE);
                            }
                        } else {
                            let handle = ctx.load_texture(format!("blur_cache_{}", blur_id), color_image, egui::TextureOptions::LINEAR);
                            let tid = handle.id();
                            self.blur_cache.insert(blur_id, BlurCacheEntry{ padded_rect: padded, radius, blurred: patch, rev: cur_rev, handle });
                            painter.image(tid, r, egui::Rect::from_min_max(egui::pos2(0.0,0.0), egui::pos2(1.0,1.0)), egui::Color32::WHITE);
                        }
                        painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)), egui::StrokeKind::Outside);
                        if selected {
                            let b = ann.bounds();
                            let br = egui::Rect::from_min_max(egui::pos2(b.x as f32/ppp, b.y as f32/ppp), egui::pos2(b.right() as f32/ppp, b.bottom() as f32/ppp));
                            painter.rect_stroke(br, 0.0, egui::Stroke::new(1.0/ppp.max(1.0), egui::Color32::from_rgba_unmultiplied(10,132,255,180)), egui::StrokeKind::Outside);
                        }
                        continue;
                    }
                }
            }
            draw_annotation(painter, ctx, ann, ppp, selected, image, &self.mgr.annotations()[..idx]);
        }
        if let Some(state) = &self.editing_text {
            let to_pt = |p: (f32,f32)| egui::pos2(p.0/ppp, p.1/ppp);
            let r = egui::Rect::from_min_max(to_pt((state.rect.x as f32, state.rect.y as f32)), to_pt((state.rect.right() as f32, state.rect.bottom() as f32)));
            painter.rect_stroke(r, 2.0, egui::Stroke::new(1.2/ppp.max(1.0), egui::Color32::from_rgba_unmultiplied(10,132,255,180)), egui::StrokeKind::Outside);
            let mx = (r.min.x + r.max.x)*0.5; let my = (r.min.y + r.max.y)*0.5;
            for pt in [r.min, egui::pos2(mx, r.min.y), egui::pos2(r.max.x, r.min.y), egui::pos2(r.max.x, my), r.max, egui::pos2(mx, r.max.y), egui::pos2(r.min.x, r.max.y), egui::pos2(r.min.x, my)] {
                painter.rect_filled(egui::Rect::from_center_size(pt, egui::vec2(6.0,6.0)), 1.0, egui::Color32::from_rgb(10,132,255));
                painter.rect_stroke(egui::Rect::from_center_size(pt, egui::vec2(6.0,6.0)), 1.0, egui::Stroke::new(1.0, egui::Color32::WHITE), egui::StrokeKind::Outside);
            }
        }
        if let Some(a) = self.mgr.in_progress().cloned() {
            // 在途马赛克模糊：走 preview_blur 专属缓存（复用 TextureHandle，避免每帧新建纹理）
            if let Annotation::Mosaic{ rect, style: crate::annotation::MosaicStyle::Blur{ radius }, .. } = &a {
                if let Some(img) = image {
                    if rect.width < 2 || rect.height < 2 {
                        let r = egui::Rect::from_min_max(egui::pos2(rect.x as f32/ppp, rect.y as f32/ppp), egui::pos2(rect.right() as f32/ppp, rect.bottom() as f32/ppp));
                        painter.rect_filled(r, 0.0, egui::Color32::from_rgba_unmultiplied(70, 70, 70, 230));
                        painter.text(r.center(), egui::Align2::CENTER_CENTER, "模糊", egui::FontId::proportional(12.0 / ppp.max(1.0)), egui::Color32::WHITE);
                        painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)), egui::StrokeKind::Outside);
                        return;
                    }
                    let r = egui::Rect::from_min_max(egui::pos2(rect.x as f32/ppp, rect.y as f32/ppp), egui::pos2(rect.right() as f32/ppp, rect.bottom() as f32/ppp));
                    let radius_v = radius.max(1.0);
                    let mut handled = false;
                    let cur_rev = self.mgr.rev();
                    if let Some(entry) = self.preview_blur.as_mut() {
                        if entry.rev == cur_rev && (entry.radius - radius_v).abs() < 0.01 && rect.x >= entry.padded_rect.x && rect.y >= entry.padded_rect.y && rect.right() <= entry.padded_rect.right() && rect.bottom() <= entry.padded_rect.bottom() {
                            let dx = (rect.x - entry.padded_rect.x) as u32;
                            let dy = (rect.y - entry.padded_rect.y) as u32;
                            let w = rect.width; let h = rect.height;
                            let cropped = image::imageops::crop_imm(&entry.blurred, dx, dy, w, h).to_image();
                            let color_image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], cropped.as_raw());
                            entry.handle.set(color_image, egui::TextureOptions::LINEAR);
                            painter.image(entry.handle.id(), r, egui::Rect::from_min_max(egui::pos2(0.0,0.0), egui::pos2(1.0,1.0)), egui::Color32::WHITE);
                            painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)), egui::StrokeKind::Outside);
                            handled = true;
                        }
                    }
                    if !handled {
                        let padded = padded_rect_for_blur(*rect, img);
                        if padded.width >= 2 && padded.height >= 2 {
                            let x0 = padded.x as u32; let y0 = padded.y as u32;
                            let pw = padded.width; let ph = padded.height;
                            let mut patch = image::imageops::crop_imm(img, x0, y0, pw, ph).to_image();
                            crate::annotation::apply_to_image(&mut patch, self.mgr.annotations(), (padded.x, padded.y));
                            stack_blur_rgba(&mut patch, radius_v as u32);
                            let dx = (rect.x - padded.x) as u32; let dy = (rect.y - padded.y) as u32;
                            let cropped = image::imageops::crop_imm(&patch, dx, dy, rect.width, rect.height).to_image();
                            let color_image = egui::ColorImage::from_rgba_unmultiplied([rect.width as usize, rect.height as usize], cropped.as_raw());
                            if let Some(entry) = self.preview_blur.as_mut() {
                                entry.padded_rect = padded;
                                entry.radius = radius_v;
                                entry.blurred = patch;
                                entry.rev = cur_rev;
                                entry.handle.set(color_image, egui::TextureOptions::LINEAR);
                                painter.image(entry.handle.id(), r, egui::Rect::from_min_max(egui::pos2(0.0,0.0), egui::pos2(1.0,1.0)), egui::Color32::WHITE);
                            } else {
                                let handle = ctx.load_texture("preview_blur", color_image, egui::TextureOptions::LINEAR);
                                let tid = handle.id();
                                self.preview_blur = Some(BlurCacheEntry{ padded_rect: padded, radius: radius_v, blurred: patch, rev: cur_rev, handle });
                                painter.image(tid, r, egui::Rect::from_min_max(egui::pos2(0.0,0.0), egui::pos2(1.0,1.0)), egui::Color32::WHITE);
                            }
                            painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)), egui::StrokeKind::Outside);
                            // 已处理，直接返回，不再走通用路径
                            return;
                        }
                    } else {
                        return;
                    }
                }
            }
            let is_text = matches!(a, Annotation::Text{..});
            let prefix = self.mgr.annotations().to_vec();
            draw_annotation(painter, ctx, &a, ppp, is_text, image, &prefix);
        }
    }
}

fn draw_annotation(painter: &egui::Painter, _ctx: &egui::Context, ann: &Annotation, ppp: f32, selected: bool, image: Option<&image::RgbaImage>, prefix: &[Annotation]) {
    let to_pt = |p: (f32,f32)| egui::pos2(p.0/ppp, p.1/ppp);
    let stroke = |c: Color, w: f32| egui::Stroke::new(w/ppp, egui::Color32::from_rgba_unmultiplied(c.r,c.g,c.b,c.a));
    match ann {
        Annotation::Rect{ rect, color, stroke_width, .. } => {
            let r = egui::Rect::from_min_max(to_pt((rect.x as f32, rect.y as f32)), to_pt((rect.right() as f32, rect.bottom() as f32)));
            painter.rect_stroke(r, 0.0, stroke(*color,*stroke_width), egui::StrokeKind::Outside);
        }
        Annotation::Arrow{ from, to, color, stroke_width, .. } => {
            let (f,t) = (to_pt(*from), to_pt(*to));
            let st = stroke(*color,*stroke_width);
            // 线段直通尖端 + 开放 V 形两翼（与导出 draw_arrow 共用 arrow_head_wings 几何，
            // 简约线条式样无填充，见 2026-09-09 用户定稿）。两翼必须走一条
            // path（w1→t→w2）：egui Round join 自动圆角连接尖端，对齐导出的
            // tip disk；拆成两笔独立线段会在尖端留下缺口（2026-09-10 实机反馈）
            painter.line_segment([f,t], st);
            let (w1, w2) = crate::annotation::tools::arrow::arrow_head_wings(*from, *to, *stroke_width);
            painter.add(egui::Shape::line(vec![to_pt(w1), t, to_pt(w2)], st));
        }
        Annotation::Brush{ points, color, stroke_width, highlighter, .. } => {
            let pts: Vec<egui::Pos2> = points.iter().map(|&p| to_pt(p)).collect();
            if pts.len()>=2 {
                let mut st = stroke(*color,*stroke_width);
                if *highlighter { st.color = egui::Color32::from_rgba_unmultiplied(color.r,color.g,color.b,110); }
                painter.add(egui::Shape::line(pts, st));
            }
        }
        Annotation::Mosaic{ rect, style, .. } => {
            let r = egui::Rect::from_min_max(to_pt((rect.x as f32, rect.y as f32)), to_pt((rect.right() as f32, rect.bottom() as f32)));
            match style {
                crate::annotation::MosaicStyle::Pixelate{ block_size } => {
                    if let Some(img)=image {
                        let bs=(*block_size as i32).max(2);
                        let x0=rect.x.clamp(0,img.width() as i32); let y0=rect.y.clamp(0,img.height() as i32);
                        let x1=rect.right().clamp(0,img.width() as i32); let y1=rect.bottom().clamp(0,img.height() as i32);
                        if x0 < x1 && y0 < y1 {
                            // 先裁剪并把前序标注画进块（与导出 apply_to_image 顺序一致，
                            // 保证被本马赛克覆盖的下层标注预览可见），再分块均值。
                            let w = (x1-x0) as u32; let h = (y1-y0) as u32;
                            let mut patch = image::imageops::crop_imm(img, x0 as u32, y0 as u32, w, h).to_image();
                            crate::annotation::apply_to_image(&mut patch, prefix, (x0, y0));
                            for by in (0..h as i32).step_by(bs as usize) { for bx in (0..w as i32).step_by(bs as usize) {
                                let bx1=(bx+bs).min(w as i32); let by1=(by+bs).min(h as i32);
                                let mut rs=0; let mut gs=0; let mut bs_=0; let mut cnt=0;
                                for py in by..by1 { for px in bx..bx1 { let p=patch.get_pixel(px as u32, py as u32).0; rs+=p[0] as u32; gs+=p[1] as u32; bs_+=p[2] as u32; cnt+=1; }}
                                if cnt==0 {continue;}
                                let col=egui::Color32::from_rgb((rs/cnt) as u8,(gs/cnt) as u8,(bs_/cnt) as u8);
                                let lr=egui::Rect::from_min_max(to_pt(((x0+bx) as f32, (y0+by) as f32)), to_pt(((x0+bx1) as f32, (y0+by1) as f32)));
                                painter.rect_filled(lr,0.0,col);
                            }}
                            painter.rect_stroke(r,0.0,egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)), egui::StrokeKind::Outside);
                        }
                    } else { painter.rect_filled(r,0.0, egui::Color32::from_rgb(68,68,68)); painter.rect_stroke(r,0.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)), egui::StrokeKind::Outside); }
                }
                crate::annotation::MosaicStyle::Blur{ .. } => {
                    // 已由 Editor::draw_annotations 缓存路径处理，此处仅兜底占位（避免每帧新建纹理，C 残留已消除）
                    painter.rect_filled(r,0.0, egui::Color32::from_rgba_unmultiplied(70,70,70,230)); painter.text(r.center(), egui::Align2::CENTER_CENTER, "模糊", egui::FontId::proportional(12.0/ppp.max(1.0)), egui::Color32::WHITE); painter.rect_stroke(r,0.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)), egui::StrokeKind::Outside);
                }
                crate::annotation::MosaicStyle::Solid{ color } => { painter.rect_filled(r,0.0, egui::Color32::from_rgba_unmultiplied(color.r,color.g,color.b,255)); painter.rect_stroke(r,0.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)), egui::StrokeKind::Outside); }
            }
        }
        Annotation::Text{ rect, content, color, font_size, bold, font, .. } => {
            let r = egui::Rect::from_min_max(to_pt((rect.x as f32, rect.y as f32)), to_pt((rect.right() as f32, rect.bottom() as f32)));
            // 文本框无底色（透明），仅边框与文字，避免遮挡截图内容
            // 按行 wrapping 绘制（与导出一致的简易 wrap）
            // 文字标注预览用"标注字体" family（独立字体优先，与导出 CPU 渲染同字体文件）
            let fam = crate::ui::gui::ensure_annotation_family(_ctx, font.as_deref());
            let font_id = egui::FontId::new(font_size / ppp, fam);
            let col = egui::Color32::from_rgba_unmultiplied(color.r,color.g,color.b,color.a);
            let wrap_w = (rect.width as f32 / ppp).max(20.0);
            let galley = painter.layout(content.clone(), font_id.clone(), col, wrap_w);
            if *bold {
                // 粗体：四向 0.8px 偏移叠加，明显加粗（导出端为字体+描边，此处视觉对齐）
                let shadow = egui::Color32::from_rgba_unmultiplied(color.r,color.g,color.b, (color.a as f32 * 0.9) as u8);
                for (dx,dy) in [(0.8,0.0),(0.0,0.8),(0.8,0.8)] {
                    painter.galley(r.min + egui::vec2(dx, dy), galley.clone(), shadow);
                }
            }
            painter.galley(r.min, galley, col);
            if selected {
                painter.rect_stroke(r, 2.0, egui::Stroke::new(1.2/ppp.max(1.0), egui::Color32::from_rgba_unmultiplied(10,132,255,180)), egui::StrokeKind::Outside);
                // 八句柄（四角+四边中点）
                let mx = (r.min.x + r.max.x) * 0.5;
                let my = (r.min.y + r.max.y) * 0.5;
                for pt in [r.min, egui::pos2(mx, r.min.y), egui::pos2(r.max.x, r.min.y), egui::pos2(r.max.x, my), r.max, egui::pos2(mx, r.max.y), egui::pos2(r.min.x, r.max.y), egui::pos2(r.min.x, my)] {
                    painter.rect_filled(egui::Rect::from_center_size(pt, egui::vec2(6.0,6.0)), 1.0, egui::Color32::from_rgb(10,132,255));
                    painter.rect_stroke(egui::Rect::from_center_size(pt, egui::vec2(6.0,6.0)), 1.0, egui::Stroke::new(1.0, egui::Color32::WHITE), egui::StrokeKind::Outside);
                }
            }
        }
    }
    if selected {
        // 非文本的通用选中框已在各分支处理；文本已在分支内画，此处跳过避免双重
        if !matches!(ann, Annotation::Text{..}) {
            let b = ann.bounds();
            let r = egui::Rect::from_min_max(to_pt((b.x as f32, b.y as f32)), to_pt((b.right() as f32, b.bottom() as f32)));
            painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0/ppp.max(1.0), egui::Color32::from_rgba_unmultiplied(10,132,255,180)), egui::StrokeKind::Outside);
        }
    }
}

fn padded_rect_for_blur(rect: Rect, image: &image::RgbaImage) -> Rect {
    let pad_w = (rect.width as f32 * 0.3).max(24.0) as i32;
    let pad_h = (rect.height as f32 * 0.3).max(24.0) as i32;
    let x0 = (rect.x - pad_w).max(0);
    let y0 = (rect.y - pad_h).max(0);
    let x1 = (rect.right() + pad_w).min(image.width() as i32);
    let y1 = (rect.bottom() + pad_h).min(image.height() as i32);
    Rect::from_points(x0, y0, x1, y1)
}

fn stack_blur_rgba(patch: &mut image::RgbaImage, radius: u32) {
    let w = patch.width();
    let h = patch.height();
    let stride = w * 4;
    // libblur stack_blur 要求 radius 2..254，内部会 clamp
    stack_blur(patch.as_mut(), stride, w, h, radius, FastBlurChannels::Channels4, ThreadingPolicy::Single);
}
