//! 全屏选区覆盖层窗口（仅 Windows 平台编译）。
//!
//! 单窗口切换架构（Snipaste 式，见 PROGRESS.md 决策记录）：
//! 同一窗口内 `Selecting`（截图 + 半透明遮罩 + 拖动选区）→
//! `Preview`（遮罩加深 + 选区高亮 + 工具条）→ `Edit`（标注编辑）三种
//! 模式切换，不做销毁重建。Esc 取消、Enter / Ctrl+C 复制、Ctrl+S 保存，
//! 编辑态另支持 Ctrl+Z 撤销 / Ctrl+Shift+Z 重做。
//!
//! 窗口层级：`WS_EX_TOPMOST`（with_window_level）+
//! `WS_EX_TOOLWINDOW`（with_skip_taskbar）；`WS_EX_NOACTIVATE` 暂不启用——
//! 覆盖层依赖键盘焦点接收 Esc/Enter（见 PROGRESS.md 已知问题）。
//!
//! 坐标约定：全程物理像素（截图按物理分辨率存储，选区物理坐标可直接裁剪），
//! 仅在 egui 绘制时 ÷ scale_factor 转逻辑坐标（AGENTS.md 3.3 节）。

use std::sync::Arc;

use anyhow::Context;
use winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::platform::windows::WindowAttributesExtWindows;
use winit::window::{Window, WindowId, WindowLevel};

use crate::annotation;
use crate::config::{Config, SaveFormat, SaveMode};
use crate::utils::math::{self, Rect};
use crate::utils::{clipboard, image_codec, paths, time};

use super::editor::Editor;
use super::gui::GuiState;
use super::toolbar::{self, ToolbarAction};

/// 覆盖层模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// 选择中：截图 + 半透明遮罩，等待拖动选区。
    Selecting,
    /// 已选定：遮罩加深、选区高亮 + 工具条，等待确认（复制/保存/取消/进入标注）。
    Preview,
    /// 标注编辑中：选区锁定，画布接受标注笔画，工具条高亮当前工具。
    Edit,
}

/// 选区最小边长（物理像素），小于此值视为无效拖动。
const MIN_SELECTION_SIZE: u32 = 3;

/// 截图负载（捕获线程产出，经 `EventLoopProxy` 传回主线程）。
pub struct CapturedShot {
    /// HDR 转换后的 sRGB 截图（与最终输出一致，所见即所得；SDR 预览用）。
    pub img: image::RgbaImage,
    /// 原始 scRGB 帧（HDR 预览直通显示用；SDR 屏不用）。
    pub raw: crate::capture::frame::RawFrame,
    /// 来源显示器是否处于 HDR 模式（决定覆盖层输出路径）。
    pub is_hdr: bool,
    /// SDR 白点 scRGB 值（= nit / 80）。HDR 预览时 egui UI 层按此提升亮度，
    /// 使其与截图内容中的 SDR 白同亮（否则 UI 暗数倍）；SDR 屏为 1.0。
    pub sdr_white_scrgb: f32,
    /// 来源显示器物理矩形（覆盖层窗口铺满范围，含任务栏区域）。
    pub monitor_rect: Rect,
}

/// 覆盖层窗口：截图显示、选区交互、复制/保存动作。
pub struct Overlay {
    window: Arc<Window>,
    gui: GuiState,
    image: Arc<image::RgbaImage>,
    /// 截图纹理（须存活至 Overlay 销毁）。仅 SDR 路径使用，
    /// HDR 路径纹理存于 `GuiState` 合成管线内。
    _texture: Option<wgpu::Texture>,
    _texture_view: Option<wgpu::TextureView>,
    texture_id: Option<egui::TextureId>,
    /// HDR 输出模式（截图由合成 pass 直通显示，不画 egui Image）。
    hdr_mode: bool,
    mode: Mode,
    /// 标注编辑器（Edit 模式的画布状态与标注数据）。
    editor: Editor,
    /// 最近一次光标位置（物理像素；`MouseInput` 事件不带坐标，以此补足）。
    current_cursor: Option<(f32, f32)>,
    /// 拖动起点（物理像素）。
    drag_start: Option<(f32, f32)>,
    /// 当前选区（物理像素，等于图像像素坐标）。
    selection: Option<Rect>,
    /// 显示器物理矩形（选区边界）。
    monitor_rect: Rect,
    /// 当前修饰键状态（判断 Ctrl+C / Ctrl+S）。
    modifiers: ModifiersState,
    config: Arc<Config>,
    /// 工具条上一帧的实际渲染矩形（egui 逻辑点，遮罩挖洞用；
    /// 首帧测量后帧间复用，选区确定后位置固定不变）。
    bar_rect_cache: Option<egui::Rect>,
    /// 请求退出（Esc / Enter / 复制 / 保存后置位，宿主负责销毁）。
    pub exit_requested: bool,
}

impl Overlay {
    /// 创建覆盖层窗口（无边框、置顶、跳任务栏、铺满目标显示器）。
    ///
    /// * `event_loop` - winit 活动事件循环。
    /// * `monitor_rect` - 目标显示器物理矩形（`GetMonitorInfoW` rcMonitor）。
    pub fn create_window(
        event_loop: &ActiveEventLoop,
        monitor_rect: &Rect,
    ) -> anyhow::Result<Arc<Window>> {
        let attrs = Window::default_attributes()
            .with_title("PrismaSnap Overlay")
            .with_decorations(false)
            .with_resizable(false)
            .with_maximized(false)
            .with_position(winit::dpi::PhysicalPosition::new(
                monitor_rect.x,
                monitor_rect.y,
            ))
            .with_inner_size(winit::dpi::Size::Physical(winit::dpi::PhysicalSize::new(
                monitor_rect.width,
                monitor_rect.height,
            )))
            // WS_EX_TOPMOST：防止被其他全屏程序遮挡
            .with_window_level(WindowLevel::AlwaysOnTop)
            // WS_EX_TOOLWINDOW：不闪现任务栏/Alt+Tab
            .with_skip_taskbar(true)
            .with_undecorated_shadow(false)
            // 先隐藏创建：GPU 初始化 + 首帧渲染期间窗口不可见，
            // 避免露出未渲染的白色默认背景（用户看到黑白闪烁的根源）
            .with_visible(false);
        event_loop
            .create_window(attrs)
            .context("创建覆盖层窗口失败")
            .map(Arc::new)
    }

    /// 初始化覆盖层（上传截图纹理，进入 `Selecting` 模式）。
    ///
    /// HDR 屏走 scRGB 直通合成管线（预览完整高光），SDR 屏上传 sRGB 纹理走 egui。
    pub fn new(
        window: Arc<Window>,
        shot: CapturedShot,
        config: Arc<Config>,
    ) -> anyhow::Result<Self> {
        let mut gui = GuiState::new(&window, shot.is_hdr)?;
        let (texture_id, texture, view) = if gui.is_hdr() {
            gui.upload_scrgb_texture(&shot.raw)?;
            // UI 层亮度提升到显示器 SDR 白点（否则工具条/标注比截图内容暗数倍）
            gui.set_ui_boost(shot.sdr_white_scrgb);
            (None, None, None)
        } else {
            let (id, t, v) = gui.upload_texture(&shot.img);
            (Some(id), Some(t), Some(v))
        };
        let hdr_mode = gui.is_hdr();
        Ok(Self {
            window,
            gui,
            image: Arc::new(shot.img),
            _texture: texture,
            _texture_view: view,
            texture_id,
            hdr_mode,
            mode: Mode::Selecting,
            editor: Editor::new(),
            current_cursor: None,
            drag_start: None,
            selection: None,
            monitor_rect: shot.monitor_rect,
            modifiers: ModifiersState::empty(),
            config,
            bar_rect_cache: None,
            exit_requested: false,
        })
    }

    /// 窗口 id（宿主按 id 分发事件）。
    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    /// 事件入口：先喂 egui 记录输入，再处理业务逻辑与重绘。
    pub fn on_window_event(&mut self, event: &WindowEvent) {
        // egui 消费的事件（如工具条按钮点击）不再走覆盖层业务逻辑
        let consumed = self.gui.on_window_event(self.window.as_ref(), event);
        match event {
            WindowEvent::CloseRequested => self.exit_requested = true,
            WindowEvent::Resized(size) => self.gui.resize(size.width, size.height),
            WindowEvent::ModifiersChanged(state) => self.modifiers = state.state(),
            WindowEvent::CursorMoved { position, .. } => {
                self.current_cursor = Some((position.x as f32, position.y as f32));
                // 拖动中实时更新选区 / 进行中的标注笔画
                if self.drag_start.is_some() {
                    self.update_selection_from_drag();
                }
                if self.mode == Mode::Edit && self.editor.is_stroking() {
                    self.editor.update_stroke((position.x as f32, position.y as f32));
                    self.window.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => self.on_press(consumed),
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => self.on_release(),
            WindowEvent::KeyboardInput { event, .. } => self.on_key(event),
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }

    /// 按下左键：`Preview` 重置选区回到 `Selecting`（重新框选）；
    /// `Edit` 开始标注笔画；`Selecting` 开始拖动选区。
    ///
    /// * `egui_consumed` - 事件已被 egui 消费（点在工具条上）时不做画布处理。
    fn on_press(&mut self, egui_consumed: bool) {
        if egui_consumed {
            return;
        }
        match self.mode {
            Mode::Preview => {
                self.mode = Mode::Selecting;
                self.selection = None;
                self.drag_start = self.current_cursor;
            }
            Mode::Edit => {
                if let Some(cursor) = self.current_cursor {
                    self.editor.begin_stroke(cursor);
                }
            }
            Mode::Selecting => self.drag_start = self.current_cursor,
        }
        self.window.request_redraw();
    }

    /// 释放左键：`Selecting` 选区有效则进入 `Preview`；`Edit` 提交笔画。
    fn on_release(&mut self) {
        match self.mode {
            Mode::Edit => {
                self.editor.commit_stroke();
                self.window.request_redraw();
                return;
            }
            _ => self.drag_start = None,
        }
        if self
            .selection
            .is_some_and(|s| s.width >= MIN_SELECTION_SIZE && s.height >= MIN_SELECTION_SIZE)
        {
            self.mode = Mode::Preview;
        } else {
            self.selection = None;
        }
        self.window.request_redraw();
    }

    /// 由拖动起点与当前光标更新选区（钳制在显示器边界内）。
    fn update_selection_from_drag(&mut self) {
        if let (Some((x0, y0)), Some((x1, y1))) = (self.drag_start, self.current_cursor) {
            self.selection = Some(
                Rect::from_points(x0 as i32, y0 as i32, x1 as i32, y1 as i32)
                    .clamp(&self.monitor_rect),
            );
            self.window.request_redraw();
        }
    }

    /// 键盘动作：Esc 取消、Enter 复制、Ctrl+C 复制、Ctrl+S 保存、
    /// Ctrl+Z 撤销、Ctrl+Shift+Z 重做（后两者仅编辑态）。
    fn on_key(&mut self, key: &KeyEvent) {
        if key.state != ElementState::Pressed {
            return;
        }
        let has_selection = matches!(self.mode, Mode::Preview | Mode::Edit);
        match key.physical_key {
            PhysicalKey::Code(KeyCode::Escape) => {
                self.exit_requested = true;
            }
            PhysicalKey::Code(KeyCode::Enter) => {
                if has_selection {
                    self.copy_and_exit();
                }
            }
            PhysicalKey::Code(KeyCode::KeyC) if self.modifiers.control_key() => {
                if has_selection {
                    self.copy_and_exit();
                }
            }
            PhysicalKey::Code(KeyCode::KeyS)
                if self.modifiers.control_key() && has_selection =>
            {
                self.save_and_exit();
            }
            PhysicalKey::Code(KeyCode::KeyZ)
                if self.modifiers.control_key() && self.mode == Mode::Edit =>
            {
                if self.modifiers.shift_key() {
                    self.editor.redo();
                } else {
                    self.editor.undo();
                }
                self.window.request_redraw();
            }
            _ => {}
        }
    }

    /// 裁剪选区图像（物理坐标 = 图像像素坐标，直接裁剪）。
    ///
    /// 裁剪后把已提交标注 CPU 重绘上去（方案 B 导出端，见
    /// [`annotation::apply_to_image`]；骨架阶段为接通管线，逐工具落地）。
    fn crop_selection(&self) -> Option<image::RgbaImage> {
        let sel = self.selection?;
        let mut img = image::imageops::crop_imm(
            self.image.as_ref(),
            sel.x as u32,
            sel.y as u32,
            sel.width,
            sel.height,
        )
        .to_image();
        annotation::apply_to_image(&mut img, self.editor.annotations(), (sel.x, sel.y));
        Some(img)
    }

    /// 复制选区到剪贴板并请求退出。
    fn copy_and_exit(&mut self) {
        match self.crop_selection() {
            Some(img) => {
                if let Err(e) = clipboard::copy_image(&img) {
                    tracing::error!("复制到剪贴板失败: {e:#}");
                } else {
                    tracing::info!("已复制选区到剪贴板");
                }
            }
            None => tracing::warn!("复制请求但选区为空"),
        }
        self.exit_requested = true;
    }

    /// 保存选区到文件并请求退出（按配置的保存行为）。
    ///
    /// 「始终询问」模式先弹系统保存对话框；用户取消或保存失败时**不退出**，
    /// 留在 Preview 模式让用户重试（焦点恢复到覆盖层窗口）。
    fn save_and_exit(&mut self) {
        let Some(img) = self.crop_selection() else {
            tracing::warn!("保存请求但选区为空");
            return;
        };
        // AlwaysAsk 模式弹对话框前临时隐藏 topmost 窗口，避免遮挡模态对话框
        let hide = self.config.save.mode == SaveMode::AlwaysAsk;
        if hide {
            self.window.set_visible(false);
        }
        let result = save_shot(&img, &self.config);
        if hide {
            self.window.set_visible(true);
        }
        match result {
            Ok(Some(path)) => {
                tracing::info!("已保存选区: {}", path.display());
                self.exit_requested = true;
            }
            Ok(None) => {
                tracing::info!("用户取消保存");
                self.window.focus_window();
            }
            Err(e) => {
                tracing::error!("保存失败: {e:#}");
                self.window.focus_window();
            }
        }
    }

    /// 渲染一帧：截图 + 遮罩 + 选区 + 标注预览 + 工具条。
    ///
    /// 公开给宿主：打开覆盖层时在窗口显示前先同步渲染首帧，
    /// 避免露出未渲染的默认背景（闪烁）。
    pub fn redraw(&mut self) {
        let texture_id = self.texture_id;
        let hdr_mode = self.hdr_mode;
        let img_size = self.image.dimensions();
        let mode = self.mode;
        let selection = self.selection;
        let monitor_rect = self.monitor_rect;
        // 工具条点击动作在渲染闭包外统一处理（需 &mut self）
        let mut action: Option<ToolbarAction> = None;
        let theme = self.config.ui.theme;
        let editor = &mut self.editor;
        let window = self.window.clone();
        // 工具条矩形（egui 逻辑点）：遮罩挖洞用，保证工具条浮在原始画面上
        // 而非压暗区内（2026-08-22 用户反馈）。首帧无缓存时暂不挖洞，
        // 渲染后拿到实际矩形会主动请求再绘一帧补上
        let cached_bar_rect = self.bar_rect_cache;
        let mut bar_actual: Option<egui::Rect> = None;
        self.gui.render(window.as_ref(), |ui| {
            super::gui::apply_theme(ui.ctx(), theme);
            draw_frame(
                ui,
                texture_id,
                hdr_mode,
                img_size,
                mode,
                selection,
                monitor_rect,
                cached_bar_rect,
            );
            // 编辑态：egui 层绘制标注预览（方案 B 预览端）
            if mode == Mode::Edit {
                let ppp = ui.ctx().pixels_per_point();
                let painter = ui.ctx().layer_painter(egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("annotations"),
                ));
                editor.draw_annotations(&painter, ppp);
            }
            // 工具条：Preview / Edit 均显示（Selecting 不显示）
            if let Some(sel) = selection.filter(|_| mode != Mode::Selecting) {
                let ppp = ui.ctx().pixels_per_point();
                let pos = toolbar::toolbar_pos_pts(&sel, &monitor_rect, toolbar::BAR_SIZE, ppp);
                action = toolbar::toolbar_ui(
                    ui.ctx(),
                    pos,
                    editor.active_tool(),
                    editor.stroke_color(),
                    editor.stroke_width(),
                    editor.can_undo(),
                    editor.can_redo(),
                    &mut bar_actual,
                );
            }
        });
        // 缓存工具条实际渲染矩形；首帧测量到边界后请求再绘一帧，
        // 让贴合的遮罩挖洞立即生效
        let first_measure = self.bar_rect_cache.is_none() && bar_actual.is_some();
        self.bar_rect_cache = bar_actual;
        if first_measure {
            window.request_redraw();
        }
        if let Some(a) = action {
            self.handle_toolbar_action(a);
        }
    }

    /// 响应工具条动作：切换工具 / 撤销重做 / 复制 / 保存 / 取消。
    fn handle_toolbar_action(&mut self, action: ToolbarAction) {
        match action {
            ToolbarAction::ActivateTool(tool) => {
                if self.editor.active_tool() == Some(tool) {
                    // 再点当前工具 → 退出编辑态回到预览
                    self.editor.deactivate();
                    self.mode = Mode::Preview;
                } else {
                    self.editor.activate(tool);
                    self.mode = Mode::Edit;
                }
                self.window.request_redraw();
            }
            ToolbarAction::SetColor(color) => {
                self.editor.set_stroke_color(color);
                self.window.request_redraw();
            }
            ToolbarAction::SetStrokeWidth(width) => {
                self.editor.set_stroke_width(width);
                self.window.request_redraw();
            }
            ToolbarAction::Undo => {
                self.editor.undo();
                self.window.request_redraw();
            }
            ToolbarAction::Redo => {
                self.editor.redo();
                self.window.request_redraw();
            }
            ToolbarAction::Copy => self.copy_and_exit(),
            ToolbarAction::Save => self.save_and_exit(),
            ToolbarAction::Cancel => self.exit_requested = true,
        }
    }
}

/// 物理矩形 → egui 逻辑矩形。
fn to_pts(rect: &Rect, ppp: f32) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(rect.x as f32 / ppp, rect.y as f32 / ppp),
        egui::pos2(rect.right() as f32 / ppp, rect.bottom() as f32 / ppp),
    )
}

/// 覆盖层一帧的 egui 绘制。
///
/// HDR 模式下截图不在这里绘制（由合成 pass 直通显示），egui 只画遮罩/选区/提示层。
///
/// `bar_rect`：工具条矩形（egui 逻辑点）。Preview / Edit 模式下从遮罩中挖除，
/// 让工具条浮在原始亮度的画面上（Selecting 无工具条，传 `None`）。
fn draw_frame(
    ui: &mut egui::Ui,
    texture_id: Option<egui::TextureId>,
    hdr_mode: bool,
    img_size: (u32, u32),
    mode: Mode,
    selection: Option<Rect>,
    monitor_rect: Rect,
    bar_rect: Option<egui::Rect>,
) {
    let ctx = ui.ctx().clone();
    let ppp = ctx.pixels_per_point();
    let screen_rect = to_pts(&monitor_rect, ppp);

    // 截图（物理分辨率 1:1 铺满）；HDR 模式由合成 pass 绘制，这里跳过
    if !hdr_mode {
        if let Some(texture_id) = texture_id {
            egui::Image::new((texture_id, egui::vec2(img_size.0 as f32, img_size.1 as f32)))
                .paint_at(ui, screen_rect);
        }
    }

    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("overlay_mask"),
    ));

    match mode {
        Mode::Selecting => match selection {
            Some(sel) => {
                let sel_pts = to_pts(&sel, ppp);
                dim_outside_selection(&painter, sel_pts, screen_rect, 110, None);
                // 选区边框 + 尺寸标签
                painter.rect_stroke(
                    sel_pts,
                    0.0,
                    egui::Stroke::new(1.5, egui::Color32::from_rgb(0, 140, 255)),
                    egui::StrokeKind::Outside,
                );
                draw_size_label(&painter, &sel, sel_pts);
            }
            None => {
                // 全屏遮罩 + 操作提示
                painter.rect_filled(screen_rect, 0.0, egui::Color32::from_black_alpha(90));
                painter.text(
                    screen_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "拖动鼠标选择区域（Esc 取消）",
                    egui::FontId::proportional(18.0),
                    egui::Color32::WHITE,
                );
            }
        },
        // Preview / Edit：遮罩加深突出选区（复制/保存等动作走工具条，
        // 标注预览在 draw_frame 之后由编辑器绘制）
        Mode::Preview | Mode::Edit => {
            if let Some(sel) = selection {
                let sel_pts = to_pts(&sel, ppp);
                // 更深遮罩突出选区（挖洞保留选区亮度）；
                // 工具条区域从遮罩中挖除，浮在原始画面上
                dim_outside_selection(&painter, sel_pts, screen_rect, 170, bar_rect);
                painter.rect_stroke(
                    sel_pts,
                    0.0,
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(0, 140, 255)),
                    egui::StrokeKind::Outside,
                );
                draw_size_label(&painter, &sel, sel_pts);
            }
        }
    }
}

/// 选区外四块矩形半透明遮罩（egui painter 无挖洞原语，用四块拼）。
///
/// `exclude`：不画遮罩的区域（工具条上一帧实际矩形，egui 逻辑点）——
/// 工具条因此浮在**原始亮度**的画面上，不被遮罩的暗氛围吞没。
/// 挖洞仅外扩 [`BAR_HOLE_PAD`]（2pt）补偿抗锯齿软边，视觉与工具条完全贴合。
fn dim_outside_selection(
    painter: &egui::Painter,
    sel_pts: egui::Rect,
    screen_rect: egui::Rect,
    alpha: u8,
    exclude: Option<egui::Rect>,
) {
    let dark = egui::Color32::from_black_alpha(alpha);
    let blocks = [
        // 上
        egui::Rect::from_min_max(screen_rect.min, egui::pos2(screen_rect.max.x, sel_pts.min.y)),
        // 左
        egui::Rect::from_min_max(
            egui::pos2(screen_rect.min.x, sel_pts.min.y),
            egui::pos2(sel_pts.min.x, sel_pts.max.y),
        ),
        // 右
        egui::Rect::from_min_max(
            egui::pos2(sel_pts.max.x, sel_pts.min.y),
            egui::pos2(screen_rect.max.x, sel_pts.max.y),
        ),
        // 下
        egui::Rect::from_min_max(
            egui::pos2(screen_rect.min.x, sel_pts.max.y),
            screen_rect.max,
        ),
    ];
    for block in blocks {
        match exclude {
            Some(hole) => paint_block_with_rounded_hole(painter, block, hole, dark),
            None => {
                painter.rect_filled(block, 0.0, dark);
            }
        }
    }
}

/// 在 `block` 内绘制挖去**圆角**矩形 `hole` 的半透明遮罩，洞轮廓与工具条
/// 卡片完全贴合（含圆角，半径 [`toolbar::CORNER_RADIUS`]）。
///
/// 几何分解（矩形块 + 四角月牙）由跨平台纯函数
/// [`math::block_minus_rounded_hole`] 完成（含"无漏无重"密集采样单测），
/// 这里只做 egui 类型转换与填充。
fn paint_block_with_rounded_hole(
    painter: &egui::Painter,
    block: egui::Rect,
    hole: egui::Rect,
    dark: egui::Color32,
) {
    let to_rectf = |r: egui::Rect| math::RectF::new(r.left(), r.top(), r.right(), r.bottom());
    for piece in math::block_minus_rounded_hole(
        &to_rectf(block),
        &to_rectf(hole),
        toolbar::CORNER_RADIUS,
    ) {
        match piece {
            math::HolePiece::Rect(r) => {
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(r.min_x, r.min_y),
                        egui::pos2(r.max_x, r.max_y),
                    ),
                    0.0,
                    dark,
                );
            }
            math::HolePiece::Crescent(pts) => {
                painter.add(egui::Shape::Path(egui::epaint::PathShape {
                    points: pts.into_iter().map(|(x, y)| egui::pos2(x, y)).collect(),
                    closed: true,
                    fill: dark,
                    stroke: egui::epaint::PathStroke::default(),
                }));
            }
        }
    }
}

/// 选区尺寸标签（选区左上角内侧）。
fn draw_size_label(painter: &egui::Painter, sel: &Rect, sel_pts: egui::Rect) {
    let text = format!("{} x {}", sel.width, sel.height);
    let pos = egui::pos2(sel_pts.min.x + 6.0, sel_pts.min.y + 4.0);    let galley = painter.layout_no_wrap(
        text,
        egui::FontId::proportional(13.0),
        egui::Color32::WHITE,
    );
    let bg = egui::Rect::from_min_size(
        pos - egui::vec2(4.0, 2.0),
        galley.size() + egui::vec2(8.0, 4.0),
    );
    painter.rect_filled(bg, 3.0, egui::Color32::from_black_alpha(160));
    painter.galley(pos, galley, egui::Color32::WHITE);
}

/// 解析保存目录：配置为空时用 exe 目录下 `screenshots/`。
fn resolve_save_dir(dir: &std::path::PathBuf) -> std::path::PathBuf {
    if dir.as_os_str().is_empty() {
        exe_dir_fallback().join("screenshots")
    } else if dir.is_relative() {
        exe_dir_fallback().join(dir)
    } else {
        dir.clone()
    }
}

/// exe 目录，失败回退当前目录。
fn exe_dir_fallback() -> std::path::PathBuf {
    paths::exe_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

/// 按配置把截图保存到文件。
///
/// 返回值：`Ok(Some(path))` 保存成功；`Ok(None)` 用户在「始终询问」对话框里取消；
/// `Err(e)` 保存失败（IO 错误等）。
fn save_shot(
    img: &image::RgbaImage,
    config: &Config,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    let dir = resolve_save_dir(&config.save.dir);
    let name = format!(
        "PrismaSnap_{}.{}",
        time::timestamp_str(),
        match config.save.format {
            SaveFormat::Png => "png",
            SaveFormat::Jpeg => "jpg",
        }
    );
    let path = match config.save.mode {
        SaveMode::Silent => {
            std::fs::create_dir_all(&dir)?;
            dir.join(name)
        }
        SaveMode::AlwaysAsk => {
            // 保证默认目录存在，避免对话框静默回退到别的目录
            let _ = std::fs::create_dir_all(&dir);
            let mut dialog = rfd::FileDialog::new()
                .set_directory(&dir)
                .set_file_name(&name);
            dialog = match config.save.format {
                SaveFormat::Png => dialog.add_filter("PNG", &["png"]),
                SaveFormat::Jpeg => dialog.add_filter("JPEG", &["jpg", "jpeg"]),
            };
            match dialog.save_file() {
                Some(p) => p,
                None => return Ok(None),
            }
        }
    };
    save_to_path(img, config, &path)?;
    Ok(Some(path))
}

/// 按配置格式把图像写到指定路径。
fn save_to_path(img: &image::RgbaImage, config: &Config, path: &std::path::Path) -> anyhow::Result<()> {
    match config.save.format {
        SaveFormat::Png => image_codec::save_png(img, path)?,
        SaveFormat::Jpeg => image_codec::save_jpeg(img, path, config.save.jpeg_quality)?,
    }
    Ok(())
}
