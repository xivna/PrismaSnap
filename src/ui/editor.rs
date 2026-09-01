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

    /// 是否正在拖动已有标注。
    pub fn is_dragging(&self) -> bool {
        self.mgr.is_dragging()
    }

    /// 当前选中标注下标。
    pub fn selected(&self) -> Option<usize> {
        self.mgr.selected()
    }

    /// 当前描边颜色（工具条选中高亮用）。
    pub fn stroke_color(&self) -> Color {
        self.mgr.stroke_color
    }

    /// 切换当前描边颜色（对新标注生效）。
    pub fn set_stroke_color(&mut self, color: Color) {
        self.mgr.stroke_color = color;
    }

    /// 当前描边宽度（物理像素，工具条选中高亮用）。
    pub fn stroke_width(&self) -> f32 {
        self.mgr.stroke_width
    }

    /// 切换当前描边宽度（对新标注生效）。
    pub fn set_stroke_width(&mut self, width: f32) {
        self.mgr.stroke_width = width;
    }

    /// 当前遮挡样式（马赛克工具新建标注使用）。
    pub fn mosaic_style(&self) -> crate::annotation::MosaicStyle {
        self.mgr.mosaic_style.clone()
    }
    /// 切换遮挡样式（仅影响后续新建）。
    pub fn set_mosaic_style(&mut self, style: crate::annotation::MosaicStyle) {
        self.mgr.set_mosaic_style(style);
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

    // ── 选中/拖动代理（无工具默认态整体移动）───────────────────────

    /// 命中测试（返回最上层命中标注下标）。
    pub fn hit_test(&self, at: (f32, f32)) -> Option<usize> {
        self.mgr.hit_test(at)
    }

    /// 尝试开始拖动已有标注（无工具态调用），返回是否命中。
    pub fn begin_drag(&mut self, at: (f32, f32)) -> bool {
        self.mgr.begin_drag(at)
    }

    /// 拖动中更新位置。
    pub fn update_drag(&mut self, at: (f32, f32)) {
        self.mgr.update_drag(at);
    }

    /// 提交拖动（压撤销历史）。
    pub fn commit_drag(&mut self) -> bool {
        self.mgr.commit_drag()
    }

    /// 取消拖动（回滚）。
    pub fn cancel_drag(&mut self) {
        self.mgr.cancel_drag();
    }

    /// 手动选中指定下标。
    pub fn select(&mut self, index: Option<usize>) {
        self.mgr.select(index);
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
    /// 马赛克像素化/模糊预览需传入截图原图以实现所见即所得。
    pub fn draw_annotations(&self, painter: &egui::Painter, ctx: &egui::Context, ppp: f32, image: Option<&image::RgbaImage>) {
        for (idx, ann) in self.mgr.annotations().iter().enumerate() {
            let selected = self.mgr.selected() == Some(idx);
            draw_annotation(painter, ctx, ann, ppp, selected, image);
        }
        if let Some(a) = self.mgr.in_progress() {
            draw_annotation(painter, ctx, a, ppp, false, image);
        }
    }
}

/// 单条标注的 egui 预览绘制（选中态额外画虚线包围盒）。
fn draw_annotation(painter: &egui::Painter, ctx: &egui::Context, ann: &Annotation, ppp: f32, selected: bool, image: Option<&image::RgbaImage>) {
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
            // 箭头头部：终点处两条 30° 夹角短线（150° 方向指向起点侧）
            let dir = (to - from).normalized();
            if dir.length_sq() > 1e-6 {
                let head_len = 12.0 * st.width.max(1.0);
                for angle in [std::f32::consts::PI * 5.0 / 6.0, -std::f32::consts::PI * 5.0 / 6.0] {
                    let (sin, cos) = angle.sin_cos();
                    let d = egui::vec2(dir.x * cos - dir.y * sin, dir.x * sin + dir.y * cos);
                    painter.line_segment([to, to + d * head_len], st);
                }
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
        Annotation::Mosaic { rect, style } => {
            let r = egui::Rect::from_min_max(
                to_pt((rect.x as f32, rect.y as f32)),
                to_pt((rect.right() as f32, rect.bottom() as f32)),
            );
            match style {
                crate::annotation::MosaicStyle::Pixelate { block_size } => {
                    if let Some(img) = image {
                        // 真实像素化预览：按块均值采样原图，所见即所得
                        let bs = (*block_size as i32).max(2);
                        let x0 = rect.x.clamp(0, img.width() as i32);
                        let y0 = rect.y.clamp(0, img.height() as i32);
                        let x1 = rect.right().clamp(0, img.width() as i32);
                        let y1 = rect.bottom().clamp(0, img.height() as i32);
                        for by in (y0..y1).step_by(bs as usize) {
                            for bx in (x0..x1).step_by(bs as usize) {
                                let bx1 = (bx + bs).min(x1);
                                let by1 = (by + bs).min(y1);
                                let mut rs: u32 = 0; let mut gs: u32 = 0; let mut bs_: u32 = 0; let mut cnt: u32 = 0;
                                for py in by..by1 { for px in bx..bx1 {
                                    let p = img.get_pixel(px as u32, py as u32).0;
                                    rs += p[0] as u32; gs += p[1] as u32; bs_ += p[2] as u32; cnt += 1;
                                }}
                                if cnt == 0 { continue; }
                                let col = egui::Color32::from_rgb((rs/cnt) as u8, (gs/cnt) as u8, (bs_/cnt) as u8);
                                let lr = egui::Rect::from_min_max(to_pt((bx as f32, by as f32)), to_pt((bx1 as f32, by1 as f32)));
                                painter.rect_filled(lr, 0.0, col);
                            }
                        }
                        painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)), egui::StrokeKind::Outside);
                    } else {
                        painter.rect_filled(r, 0.0, egui::Color32::from_rgb(68, 68, 68));
                        painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)), egui::StrokeKind::Outside);
                    }
                }
                crate::annotation::MosaicStyle::Blur { radius } => {
                    if let Some(img) = image {
                        let x0 = rect.x.clamp(0, img.width() as i32) as u32;
                        let y0 = rect.y.clamp(0, img.height() as i32) as u32;
                        let x1 = rect.right().clamp(0, img.width() as i32) as u32;
                        let y1 = rect.bottom().clamp(0, img.height() as i32) as u32;
                        if x1 > x0 && y1 > y0 {
                            let w = x1 - x0; let h = y1 - y0;
                            let patch = image::imageops::crop_imm(img, x0, y0, w, h).to_image();
                            let blurred = image::imageops::blur(&patch, radius.max(1.0));
                            let color_image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], blurred.as_raw());
                            let tex = ctx.load_texture(format!("mosaic_blur_{}_{}_{}_{}", rect.x, rect.y, w, h), color_image, egui::TextureOptions::LINEAR);
                            painter.image(tex.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
                            painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)), egui::StrokeKind::Outside);
                            return;
                        }
                    }
                    painter.rect_filled(r, 0.0, egui::Color32::from_rgba_unmultiplied(70, 70, 70, 230));
                    painter.text(r.center(), egui::Align2::CENTER_CENTER, "模糊", egui::FontId::proportional(12.0 / ppp.max(1.0)), egui::Color32::WHITE);
                    painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)), egui::StrokeKind::Outside);
                }
                crate::annotation::MosaicStyle::Solid { color } => {
                    painter.rect_filled(r, 0.0, egui::Color32::from_rgba_unmultiplied(color.r, color.g, color.b, 255));
                    painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)), egui::StrokeKind::Outside);
                }
            }
        }
        Annotation::Text { pos, content, color, font_size } => {
            let pt = to_pt(*pos);
            // 简易文字预览：用 egui 文本（CJK 已由 gui 安装）
            painter.text(pt, egui::Align2::LEFT_TOP, content, egui::FontId::proportional(font_size / ppp), egui::Color32::from_rgba_unmultiplied(color.r, color.g, color.b, color.a));
        }
    }
    if selected {
        let b = ann.bounds();
        let r = egui::Rect::from_min_max(to_pt((b.x as f32, b.y as f32)), to_pt((b.right() as f32, b.bottom() as f32)));
        // 选中虚线框（细线+半透明蓝，工具条无选中态时的视觉反馈）
        painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0 / ppp.max(1.0), egui::Color32::from_rgba_unmultiplied(10, 132, 255, 180)), egui::StrokeKind::Outside);
    }
}
