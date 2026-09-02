//! 标注编辑器模块（仅 Windows 平台编译）。

use crate::annotation::{Annotation, AnnotationManager, Color, Tool};
use crate::utils::math::Rect;

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

/// 标注编辑器状态。
pub struct Editor {
    mgr: AnnotationManager,
    active_tool: Option<Tool>,
    editing_text: Option<TextEditState>,
    resizing_text: Option<TextResizeState>,
}

impl Editor {
    pub fn new() -> Self {
        Self { mgr: AnnotationManager::default(), active_tool: None, editing_text: None, resizing_text: None }
    }
    pub fn active_tool(&self) -> Option<Tool> { self.active_tool }
    pub fn is_editing(&self) -> bool { self.active_tool.is_some() }
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
        let Some(Annotation::Text{ rect, content, color, font_size, bold }) = self.mgr.annotations().get(index).cloned() else { return false; };
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
        self.mgr.update_stroke(at);
    }
    pub fn commit_stroke(&mut self) {
        // 文字工具：拖动创建文本框 -> 转为编辑态，不直接 push 占位
        if self.active_tool == Some(Tool::Text) {
            if let Some(Annotation::Text{ rect, .. }) = self.mgr.in_progress().cloned() {
                let r = rect;
                self.mgr.cancel_stroke(); // 丢弃 in_progress 占位
                self.begin_text_edit_with_rect(r);
                return;
            }
        }
        self.mgr.commit_stroke();
    }
    pub fn cancel_stroke(&mut self) { self.mgr.cancel_stroke(); }

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
        if self.resizing_text.is_some() { self.update_text_resize(at); } else { self.mgr.update_drag(at); }
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
        self.mgr.undo()
    }
    pub fn redo(&mut self) -> bool {
        if self.editing_text.is_some() || self.resizing_text.is_some() { return false; }
        self.mgr.redo()
    }
    pub fn can_undo(&self) -> bool { self.mgr.can_undo() }
    pub fn can_redo(&self) -> bool { self.mgr.can_redo() }
    pub fn annotations(&self) -> &[Annotation] { self.mgr.annotations() }

    pub fn draw_annotations(&self, painter: &egui::Painter, ctx: &egui::Context, ppp: f32, image: Option<&image::RgbaImage>) {
        let editing_idx = self.editing_text.as_ref().and_then(|s| s.index);
        for (idx, ann) in self.mgr.annotations().iter().enumerate() {
            if Some(idx) == editing_idx { continue; }
            let selected = self.mgr.selected() == Some(idx);
            draw_annotation(painter, ctx, ann, ppp, selected, image);
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
        if let Some(a) = self.mgr.in_progress() {
            let is_text = matches!(a, Annotation::Text{..});
            draw_annotation(painter, ctx, a, ppp, is_text, image);
        }
    }
}

fn draw_annotation(painter: &egui::Painter, ctx: &egui::Context, ann: &Annotation, ppp: f32, selected: bool, image: Option<&image::RgbaImage>) {
    let to_pt = |p: (f32,f32)| egui::pos2(p.0/ppp, p.1/ppp);
    let stroke = |c: Color, w: f32| egui::Stroke::new(w/ppp, egui::Color32::from_rgba_unmultiplied(c.r,c.g,c.b,c.a));
    match ann {
        Annotation::Rect{ rect, color, stroke_width } => {
            let r = egui::Rect::from_min_max(to_pt((rect.x as f32, rect.y as f32)), to_pt((rect.right() as f32, rect.bottom() as f32)));
            painter.rect_stroke(r, 0.0, stroke(*color,*stroke_width), egui::StrokeKind::Outside);
        }
        Annotation::Arrow{ from, to, color, stroke_width } => {
            let (f,t) = (to_pt(*from), to_pt(*to));
            let st = stroke(*color,*stroke_width);
            painter.line_segment([f,t], st);
            let dir = (t - f).normalized();
            if dir.length_sq() > 1e-6 {
                let hl = 12.0*st.width.max(1.0);
                for ang in [std::f32::consts::PI*5.0/6.0, -std::f32::consts::PI*5.0/6.0] {
                    let (s,c) = ang.sin_cos();
                    let d = egui::vec2(dir.x*c - dir.y*s, dir.x*s + dir.y*c);
                    painter.line_segment([t, t + d*hl], st);
                }
            }
        }
        Annotation::Brush{ points, color, stroke_width, highlighter } => {
            let pts: Vec<egui::Pos2> = points.iter().map(|&p| to_pt(p)).collect();
            if pts.len()>=2 {
                let mut st = stroke(*color,*stroke_width);
                if *highlighter { st.color = egui::Color32::from_rgba_unmultiplied(color.r,color.g,color.b,110); }
                painter.add(egui::Shape::line(pts, st));
            }
        }
        Annotation::Mosaic{ rect, style } => {
            let r = egui::Rect::from_min_max(to_pt((rect.x as f32, rect.y as f32)), to_pt((rect.right() as f32, rect.bottom() as f32)));
            match style {
                crate::annotation::MosaicStyle::Pixelate{ block_size } => {
                    if let Some(img)=image {
                        let bs=(*block_size as i32).max(2);
                        let x0=rect.x.clamp(0,img.width() as i32); let y0=rect.y.clamp(0,img.height() as i32);
                        let x1=rect.right().clamp(0,img.width() as i32); let y1=rect.bottom().clamp(0,img.height() as i32);
                        for by in (y0..y1).step_by(bs as usize) { for bx in (x0..x1).step_by(bs as usize) {
                            let bx1=(bx+bs).min(x1); let by1=(by+bs).min(y1);
                            let mut rs=0; let mut gs=0; let mut bs_=0; let mut cnt=0;
                            for py in by..by1 { for px in bx..bx1 { let p=img.get_pixel(px as u32, py as u32).0; rs+=p[0] as u32; gs+=p[1] as u32; bs_+=p[2] as u32; cnt+=1; }}
                            if cnt==0 {continue;}
                            let col=egui::Color32::from_rgb((rs/cnt) as u8,(gs/cnt) as u8,(bs_/cnt) as u8);
                            let lr=egui::Rect::from_min_max(to_pt((bx as f32, by as f32)), to_pt((bx1 as f32, by1 as f32)));
                            painter.rect_filled(lr,0.0,col);
                        }}
                        painter.rect_stroke(r,0.0,egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)), egui::StrokeKind::Outside);
                    } else { painter.rect_filled(r,0.0, egui::Color32::from_rgb(68,68,68)); painter.rect_stroke(r,0.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)), egui::StrokeKind::Outside); }
                }
                crate::annotation::MosaicStyle::Blur{ radius } => {
                    if let Some(img)=image {
                        let x0=rect.x.clamp(0,img.width() as i32) as u32; let y0=rect.y.clamp(0,img.height() as i32) as u32;
                        let x1=rect.right().clamp(0,img.width() as i32) as u32; let y1=rect.bottom().clamp(0,img.height() as i32) as u32;
                        if x1>x0 && y1>y0 { let w=x1-x0; let h=y1-y0; let patch=image::imageops::crop_imm(img,x0,y0,w,h).to_image(); let blurred=image::imageops::blur(&patch, radius.max(1.0)); let color_image=egui::ColorImage::from_rgba_unmultiplied([w as usize,h as usize], blurred.as_raw()); let tex=ctx.load_texture(format!("mosaic_blur_{}_{}_{}_{}",rect.x,rect.y,w,h), color_image, egui::TextureOptions::LINEAR); painter.image(tex.id(), r, egui::Rect::from_min_max(egui::pos2(0.0,0.0), egui::pos2(1.0,1.0)), egui::Color32::WHITE); painter.rect_stroke(r,0.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)), egui::StrokeKind::Outside); return; }
                    }
                    painter.rect_filled(r,0.0, egui::Color32::from_rgba_unmultiplied(70,70,70,230)); painter.text(r.center(), egui::Align2::CENTER_CENTER, "模糊", egui::FontId::proportional(12.0/ppp.max(1.0)), egui::Color32::WHITE); painter.rect_stroke(r,0.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)), egui::StrokeKind::Outside);
                }
                crate::annotation::MosaicStyle::Solid{ color } => { painter.rect_filled(r,0.0, egui::Color32::from_rgba_unmultiplied(color.r,color.g,color.b,255)); painter.rect_stroke(r,0.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)), egui::StrokeKind::Outside); }
            }
        }
        Annotation::Text{ rect, content, color, font_size, bold } => {
            let r = egui::Rect::from_min_max(to_pt((rect.x as f32, rect.y as f32)), to_pt((rect.right() as f32, rect.bottom() as f32)));
            // 文本框无底色（透明），仅边框与文字，避免遮挡截图内容
            // 按行 wrapping 绘制（与导出一致的简易 wrap）
            let font_id = egui::FontId::proportional(font_size / ppp);
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
