//! 全屏选区覆盖层窗口（仅 Windows 平台编译）。
//!
//! 单窗口切换架构（Snipaste 式，见 PROGRESS.md 决策记录）：
//! 同一窗口内 `Selecting`（截图 + 半透明遮罩 + 拖动选区）→
//! `Preview`（遮罩加深 + 选区高亮 + 工具条）→ `Edit`（标注编辑）三种
//! 模式切换，不做销毁重建。Esc 取消、Enter / Ctrl+C 复制、Ctrl+S 保存，
    //! 编辑态另支持 Ctrl+Z 撤销 / Ctrl+Shift+Z / Ctrl+Y 重做。
//!
//! 窗口层级：`WS_EX_TOPMOST`（with_window_level）+
//! `WS_EX_TOOLWINDOW`（with_skip_taskbar）；`WS_EX_NOACTIVATE` 暂不启用——
//! 覆盖层依赖键盘焦点接收 Esc/Enter（见 PROGRESS.md 已知问题）。
//!
//! 坐标约定：全程物理像素（截图按物理分辨率存储，选区物理坐标可直接裁剪），
//! 仅在 egui 绘制时 ÷ scale_factor 转逻辑坐标（AGENTS.md 3.3 节）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::platform::windows::WindowAttributesExtWindows;
use winit::window::{Window, WindowId, WindowLevel};

use crate::annotation::{self, Annotation};
use crate::config::{Config, SaveFormat, SaveMode};
use crate::ocr::{TextRegion, TranslatedRegion};
use crate::translate::merge::merge_regions_into_blocks;
use crate::translate::render;
use crate::utils::math::{self, Rect};
use crate::utils::{clipboard, image_codec, paths, time};

use super::ai::{self, AiDone};
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

/// AI 任务类型（提取文字 / 翻译共用一次 OCR，结果路由依据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AiJob {
    /// 空闲（无在途任务）。
    Idle,
    /// 提取文字（OCR 完成后进可编辑面板）。
    Extract,
    /// 翻译（OCR 完成后自动链式进翻译管线）。
    Translate,
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
    /// 工具条字体选择弹层状态（选中文字改字体，Word 式悬停实时预览）。
    font_picker: super::font_list::FontPickerState,
    /// 最近一次光标位置（物理像素；`MouseInput` 事件不带坐标，以此补足）。
    current_cursor: Option<(f32, f32)>,
    /// 拖动起点（物理像素）。
    drag_start: Option<(f32, f32)>,
    /// 当前选区（物理像素，等于图像像素坐标）。
    selection: Option<Rect>,
    /// 选区整体拖动状态（Preview 模式下点命中选区内部时进入）。
    selection_drag_start: Option<(f32, f32)>,
    selection_drag_origin: Option<Rect>,
    /// 显示器物理矩形（选区边界）。
    monitor_rect: Rect,
    /// 当前修饰键状态（判断 Ctrl+C / Ctrl+S）。
    modifiers: ModifiersState,
    config: Arc<Config>,
    /// 工具条字体选择同步全局默认后暂存的新配置（宿主负责写盘 + 热更新
    /// `App.config`；写盘 single-writer 仍在宿主，见 Bug4 2026-09-10）。
    pending_config: Option<Config>,
    /// 工具条上一帧的实际渲染矩形（egui 逻辑点，遮罩挖洞用；
    /// 首帧测量后帧间复用，选区确定后位置固定不变）。
    bar_rect_cache: Option<egui::Rect>,
    /// 请求退出（Esc / Enter / 复制 / 保存后置位，宿主负责销毁）。
    pub exit_requested: bool,
    /// 双击检测：上次点击时间与命中文本索引
    last_click: Option<(Instant, (f32, f32), usize)>,
    /// AI 完成回调用（宿主注入：接到 winit `EventLoopProxy`，见 AGENTS.md 3.10）。
    ai_notify: Option<std::sync::Arc<dyn Fn(AiDone) + Send + Sync>>,
    /// AI 请求单调序号（选区变化即自增，在途旧结果按号丢弃）。
    ai_req: u64,
    /// 当前 AI 任务类型（路由 OCR 完成后的去向）。
    ai_job: AiJob,
    /// AI 任务进行中（工具条提取/翻译按钮禁用 + 状态行 Loading）。
    ai_busy: bool,
    /// AI 任务开始时刻（状态行显示已耗时；`None` 表示空闲）。
    ai_start: Option<Instant>,
    /// AI 状态行（"识别中…"/"已复制提取的文字"/"翻译失败：…"，选区变化即清除）。
    ai_status: Option<String>,
    /// 已完成的译文覆盖（全图坐标；预览 egui 绘制 + 导出 CPU 重绘）。
    ai_translated: Vec<TranslatedRegion>,
    /// OCR 缓存：产生该结果时的选区（命中才复用，避免重复识别）。
    ai_cached_sel: Option<Rect>,
    /// OCR 缓存：全图坐标的识别区域。
    pub ai_cached_regions: Vec<TextRegion>,
    /// 内联文字输入框本帧的 egui Id（工具条提交后恢复焦点/选区用，外援 R3；
    /// `TextEdit` 无显式 id 时按 Ui 位置派生，内容变化不影响其稳定）。
    edit_text_id: Option<egui::Id>,
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
            // 不要 DWM 重定向位图（GDI 底色）。否则 set_visible 时 DWM 会先
            // 闪一帧窗口类背景，再接上 DXGI swapchain。
            .with_no_redirection_bitmap(true)
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
            font_picker: super::font_list::FontPickerState::default(),
            current_cursor: None,
            drag_start: None,
            selection: None,
            selection_drag_start: None,
            selection_drag_origin: None,
            monitor_rect: shot.monitor_rect,
            modifiers: ModifiersState::empty(),
            config,
            pending_config: None,
            bar_rect_cache: None,
            exit_requested: false,
            last_click: None,
            ai_notify: None,
            ai_req: 0,
            ai_job: AiJob::Idle,
            ai_busy: false,
            ai_start: None,
            ai_status: None,
            ai_translated: Vec::new(),
            ai_cached_sel: None,
            ai_cached_regions: Vec::new(),
            edit_text_id: None,
        })
    }

    /// 窗口 id（宿主按 id 分发事件）。
    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    /// 取出暂存的全局配置更新（宿主写盘 + 热更新后即清空，单次有效）。
    pub fn take_pending_config(&mut self) -> Option<Config> {
        self.pending_config.take()
    }

    /// 事件入口：先喂 egui 记录输入，再处理业务逻辑与重绘。
    pub fn on_window_event(&mut self, event: &WindowEvent) {
        // 文字编辑态：Esc 取消、Enter（无 Shift）确认 优先拦截，避免落到退出逻辑
        if self.editor.is_editing_text() {
            if let WindowEvent::KeyboardInput { event: key, .. } = event {
                if key.state == ElementState::Pressed {
                    match key.physical_key {
                        PhysicalKey::Code(KeyCode::Escape) => {
                            self.editor.cancel_text_edit();
                            self.window.request_redraw();
                            return;
                        }
                        PhysicalKey::Code(KeyCode::Enter)
                            // Shift+Enter 交给 egui 插换行，普通 Enter 确认
                            if !self.modifiers.shift_key() => {
                                self.editor.commit_text_edit();
                                self.window.request_redraw();
                                return;
                            }
                        _ => {}
                    }
                }
            }
        }
        // egui 消费的事件（如工具条按钮点击）不再走覆盖层业务逻辑
        let consumed = self.gui.on_window_event(self.window.as_ref(), event);
        match event {
            WindowEvent::CloseRequested => self.exit_requested = true,
            WindowEvent::Resized(size) => self.gui.resize(size.width, size.height),
            WindowEvent::ModifiersChanged(state) => self.modifiers = state.state(),
            WindowEvent::CursorMoved { position, .. } => {
                self.current_cursor = Some((position.x as f32, position.y as f32));
                if self.drag_start.is_some() {
                    self.update_selection_from_drag();
                }
                if self.editor.is_dragging() || self.editor.is_resizing_text() {
                    self.editor.update_drag((position.x as f32, position.y as f32));
                    self.window.request_redraw();
                }
                if self.selection_drag_start.is_some() {
                    self.update_selection_drag();
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
            WindowEvent::RedrawRequested => {
                let _ = self.redraw();
            }
            _ => {}
        }
    }

    fn is_double_click(&mut self, pt: (f32,f32), idx: usize) -> bool {
        let now = Instant::now();
        let is_double = if let Some((t, last_pt, last_idx)) = self.last_click {
            last_idx == idx && now.duration_since(t) < Duration::from_millis(350) && (pt.0 - last_pt.0).hypot(pt.1 - last_pt.1) < 8.0
        } else { false };
        self.last_click = Some((now, pt, idx));
        is_double
    }

    /// 按下左键：`Preview` 命中标注则拖动标注、空白处重置选区回到 `Selecting`；
    /// `Edit` 开始标注笔画 / 文字编辑；`Selecting` 开始拖动选区。
    ///
    /// * `egui_consumed` - 事件已被 egui 消费（点在工具条/文字输入框上）时不做画布处理。
    fn on_press(&mut self, egui_consumed: bool) {
        if egui_consumed {
            return;
        }
        match self.mode {
            Mode::Preview => {
                if let Some(pt) = self.current_cursor {
                    // 双击文本：未选文字工具时自动切文字工具并进入编辑
                    if let Some(idx) = self.editor.hit_test(pt) {
                        if let Some(Annotation::Text{..}) = self.editor.annotations().get(idx) {
                            if self.is_double_click(pt, idx) {
                                if self.editor.is_editing_text() { self.editor.commit_text_edit(); }
                                self.editor.activate(crate::annotation::Tool::Text);
                                self.mode = Mode::Edit;
                                self.editor.begin_text_edit_existing(idx);
                                self.window.request_redraw();
                                return;
                            }
                        } else {
                            // 非文本命中重置双击状态（避免跨标注误判）
                            // 保留 last_click 供下次判断，但命中不同 idx 已在 is_double_click 中处理
                        }
                    }
                    if self.editor.begin_drag(pt) {
                        self.window.request_redraw();
                        // 单击已记录双击时间，下次双击可进入编辑
                        return;
                    }
                    // 未命中标注：若点在选区内部则整体拖动选区
                    if let Some(sel) = self.selection {
                        if Self::point_in_rect(pt, &sel) {
                            self.selection_drag_start = Some(pt);
                            self.selection_drag_origin = Some(sel);
                            self.editor.select(None);
                            self.window.request_redraw();
                            return;
                        }
                    }
                }
                // 点在选区外：仅清除选中，不重置选区（避免误触取消选区）
                self.editor.select(None);
                self.window.request_redraw();
                return;
            }
            Mode::Edit => {
                // 未选文字工具时双击文本自动切文字工具并编辑
                if self.editor.active_tool() != Some(crate::annotation::Tool::Text) {
                    if let Some(pt) = self.current_cursor {
                        if let Some(idx) = self.editor.hit_test(pt) {
                            if let Some(Annotation::Text{..}) = self.editor.annotations().get(idx) {
                                if self.is_double_click(pt, idx) {
                                    if self.editor.is_editing_text() { self.editor.commit_text_edit(); }
                                    self.editor.activate(crate::annotation::Tool::Text);
                                    self.editor.begin_text_edit_existing(idx);
                                    self.window.request_redraw();
                                    return;
                                }
                            }
                        }
                    }
                }
                if self.editor.active_tool() == Some(crate::annotation::Tool::Text) {
                    if let Some(pt) = self.current_cursor {
                        if let Some((idx, h)) = self.editor.hit_text_handle(pt) {
                            if idx == usize::MAX {
                                self.editor.begin_text_resize(idx, h, pt);
                                self.window.request_redraw();
                                return;
                            }
                            if self.editor.is_editing_text() { self.editor.commit_text_edit(); }
                            self.editor.begin_text_resize(idx, h, pt);
                            self.window.request_redraw();
                            return;
                        }
                        // 单击文本直接进入编辑（文字工具下无需双击）
                        if let Some(idx) = self.editor.hit_test(pt) {
                            if let Some(Annotation::Text { .. }) = self.editor.annotations().get(idx) {
                                if self.editor.is_editing_text() { self.editor.commit_text_edit(); }
                                self.editor.begin_text_edit_existing(idx);
                                self.window.request_redraw();
                                return;
                            }
                        }
                        if self.editor.is_editing_text() {
                            self.editor.commit_text_edit();
                            self.window.request_redraw();
                            return;
                        }
                        if let Some(c) = self.current_cursor {
                            self.editor.begin_stroke(c);
                            self.window.request_redraw();
                            return;
                        }
                    }
                }
                if let Some(cursor) = self.current_cursor {
                    self.editor.begin_stroke(cursor);
                }
            }
            Mode::Selecting => self.drag_start = self.current_cursor,
        }
        self.window.request_redraw();
    }

    /// 释放左键：`Selecting` 选区有效则进入 `Preview`；`Edit` 提交笔画；`Preview` 拖动提交。
    fn on_release(&mut self) {
        if self.editor.is_dragging() || self.editor.is_resizing_text() {
            self.editor.commit_drag();
            self.window.request_redraw();
            return;
        }
        // 选区整体拖动
        if self.selection_drag_start.is_some() {
            self.selection_drag_start = None;
            self.selection_drag_origin = None;
            self.window.request_redraw();
            return;
        }
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
            self.invalidate_ai();
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
            self.invalidate_ai();
            self.window.request_redraw();
        }
    }

    /// 选区整体拖动更新（保持尺寸，整体平移并钳制在显示器内）。
    fn update_selection_drag(&mut self) {
        if let (Some((sx, sy)), Some((cx, cy)), Some(origin)) =
            (self.selection_drag_start, self.current_cursor, self.selection_drag_origin)
        {
            let dx = (cx - sx) as i32;
            let dy = (cy - sy) as i32;
            let moved = Rect {
                x: origin.x + dx,
                y: origin.y + dy,
                width: origin.width,
                height: origin.height,
            }
            .clamp(&self.monitor_rect);
            self.selection = Some(moved);
            self.invalidate_ai();
            self.window.request_redraw();
        }
    }

    /// 判断点是否在矩形内（含边界，物理像素）。
    fn point_in_rect(pt: (f32, f32), r: &Rect) -> bool {
        let x = pt.0 as i32;
        let y = pt.1 as i32;
        x >= r.x && x < r.right() && y >= r.y && y < r.bottom()
    }

    /// 键盘动作：Esc 取消（拖动中取消拖动、否则退出）、Enter 复制、Ctrl+C 复制、Ctrl+S 保存、
    /// Ctrl+Z 撤销、Ctrl+Shift+Z / Ctrl+Y 重做（后两者 Preview/Edit 均可）。
    fn on_key(&mut self, key: &KeyEvent) {
        if key.state != ElementState::Pressed {
            return;
        }
        // 文字输入中全局快捷键不生效（Enter/Esc 已在 on_window_event 拦截）
        if self.editor.is_editing_text() {
            return;
        }
        let has_selection = matches!(self.mode, Mode::Preview | Mode::Edit);
        match key.physical_key {
            PhysicalKey::Code(KeyCode::Escape) => {
                if self.editor.is_dragging() {
                    self.editor.cancel_drag();
                    self.window.request_redraw();
                    return;
                }
                if self.editor.is_stroking() {
                    self.editor.deactivate();
                    self.window.request_redraw();
                    return;
                }
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
                if self.modifiers.control_key() && matches!(self.mode, Mode::Preview | Mode::Edit) =>
            {
                if self.modifiers.shift_key() {
                    self.editor.redo();
                } else {
                    self.editor.undo();
                }
                self.window.request_redraw();
            }
            // Ctrl+Y 重做（与 Ctrl+Shift+Z 同义：部分键盘/输入法下 Shift+Z 难按，
            // 且符合 Office/PS 用户习惯；Ctrl+Shift+Z 保留）。
            PhysicalKey::Code(KeyCode::KeyY)
                if self.modifiers.control_key() && matches!(self.mode, Mode::Preview | Mode::Edit) =>
            {
                self.editor.redo();
                self.window.request_redraw();
            }
            _ => {}
        }
    }

    /// 裁剪选区图像（物理坐标 = 图像像素坐标，直接裁剪）。
    ///
    /// 先把译文覆盖 CPU 重绘上去（[`render::render_translated_region`]），再把
    /// 已提交标注 CPU 重绘上去（方案 B 导出端，见 [`annotation::apply_to_image`]），
    /// 顺序与预览一致（译文在下、标注在上）。
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
        for r in &self.ai_translated {
            render::render_translated_region(&mut img, r, false, (sel.x, sel.y));
        }
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
    ///
    /// 返回本帧是否成功 present（swapchain 未就绪时为 `false`）。
    pub fn redraw(&mut self) -> bool {
        let texture_id = self.texture_id;
        let hdr_mode = self.hdr_mode;
        let img_size = self.image.dimensions();
        let mode = self.mode;
        let selection = self.selection;
        let monitor_rect = self.monitor_rect;
        // 工具条点击动作在渲染闭包外统一处理（需 &mut self）
        let mut action: Option<ToolbarAction> = None;
        // 状态行取消按钮同样闭包外执行
        let mut status_cancel = false;
        // 「复制译文」按钮同样闭包外执行（翻译成功后才出现，见下）
        let mut copy_translated = false;
        let theme = self.config.ui.theme;
        let editor = &mut self.editor;
        let mut font_picker_state = std::mem::take(&mut self.font_picker);
        // 帧开始前预注册本帧要用到的逐标注字体（egui 的 set_fonts 在帧中间调用
        // 当帧不生效，沿用旧字体表的 galley 会对未绑定 family 直接 panic——
        // 2026-09-09 实机闪退 "FontFamily::Name(ann_..) is not bound to any fonts"）
        // 常规与粗体变体都预注册（富文本逐字符字体/加粗各用各 family，预览 LayoutJob
        // 帧内 ensure 只是复用已注册项，不触发 set_fonts）。
        for font_path in editor.all_text_fonts() {
            super::gui::ensure_annotation_family(self.gui.egui_ctx(), Some(&font_path));
            super::gui::ensure_annotation_family_bold(self.gui.egui_ctx(), Some(&font_path));
        }
        // 编辑态草稿加粗变体同样帧前预注册（有变体才用真粗体 family 渲染输入框）
        if let Some(bold_font) = editor.editing_bold_font() {
            super::gui::ensure_annotation_family_bold(self.gui.egui_ctx(), bold_font.as_deref());
        }
        let ai_busy = self.ai_busy;
        let ai_start = self.ai_start;
        let ai_translated = &self.ai_translated;
        let ai_status = &self.ai_status;
        let window = self.window.clone();
        // 工具条矩形（egui 逻辑点）：遮罩挖洞用，保证工具条浮在原始画面上
        // 而非压暗区内（2026-08-22 用户反馈）。首帧无缓存时暂不挖洞，
        // 渲染后拿到实际矩形会主动请求再绘一帧补上
        let cached_bar_rect = self.bar_rect_cache;
        let mut bar_actual: Option<egui::Rect> = None;
        // 内联输入框本帧 Id（渲染闭包内回填，见下；未编辑时保持 None）
        let mut edit_text_id: Option<egui::Id> = None;
        let presented = self.gui.render(window.as_ref(), |ui| {
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
            // Preview / Edit 均绘制已提交标注与选中高亮（拖动态在 Preview，像素化/模糊需原图实现所见即所得）
            if matches!(mode, Mode::Preview | Mode::Edit) {
                let ppp = ui.ctx().pixels_per_point();
                let ctx = ui.ctx().clone();
                // 标注层用 Middle，确保工具条（Tooltip）始终在最上层不被遮挡
                let painter = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Middle,
                    egui::Id::new("annotations"),
                ));
                let img_ref: Option<&image::RgbaImage> = Some(self.image.as_ref());
                // 同步选区到编辑器用于钳制与裁剪（防止拖出选区外遮挡工具条）
                editor.set_selection(selection);
                // 译文覆盖预览（用户标注之下；导出时同顺序重绘，保证一致）
                Self::draw_translated_preview(&painter, ppp, ai_translated);
                editor.draw_annotations(&painter, &ctx, ppp, img_ref);
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
                    &editor.mosaic_style(),
                    editor.text_font_size(),
                    editor.text_bold(),
                    // 选中/编辑中文字的独立字体（编辑态恒 Some，新建输入中也可用；
                    // None = 无目标，字体选择器置灰）
                    editor.selected_text_font(),
                    editor.selected_text_style(),
                    &mut font_picker_state,
                    editor.can_undo(),
                    editor.can_redo(),
                    ai_busy,
                    &mut bar_actual,
                );
            }
            // AI 状态行（等待态：转圈 + 分阶段文案 + 已耗时 + 取消；
            // 错误提示纯文本；翻译成功后附「复制译文」按钮（译文只是像素，
            // 不可选中，按钮复制全部译文）。浮在尺寸标签之上（两者都在选区左上外侧，
            // 差 28px 错开互不遮挡）；顶部没地儿才压进选区内）
            // 翻译成功时 `ai_status` 为 None（不挡译文），但复制按钮仍要出现，
            // 故条件为"有状态文本，或有可复制的译文"。
            let can_copy = !ai_busy && !ai_translated.is_empty();
            if ai_status.is_some() || can_copy {
                if let Some(sel) = selection {
                    let ppp = ui.ctx().pixels_per_point();
                    let sx = sel.x as f32 / ppp;
                    let top = monitor_rect.y as f32 / ppp;
                    let mut sy = sel.y as f32 / ppp - 58.0;
                    if sy < top {
                        sy = sel.y as f32 / ppp + 4.0;
                    }
                    egui::Area::new(egui::Id::new("ai_status"))
                        .fixed_pos(egui::pos2(sx, sy))
                        .order(egui::Order::Tooltip)
                        .show(ui.ctx(), |ui| {
                            egui::Frame::new()
                                .fill(egui::Color32::from_black_alpha(200))
                                .corner_radius(6.0)
                                .inner_margin(egui::Margin::symmetric(8, 4))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        if ai_busy {
                                            ui.add(egui::Spinner::new().size(14.0));
                                        }
                                        if let Some(status) = ai_status {
                                            let mut text = status.clone();
                                            if ai_busy {
                                                if let Some(t0) = ai_start {
                                                    let secs = Instant::now()
                                                        .duration_since(t0)
                                                        .as_secs();
                                                    text.push_str(&format!("（{}s）", secs));
                                                }
                                            }
                                            ui.label(
                                                egui::RichText::new(text)
                                                    .color(egui::Color32::WHITE)
                                                    .size(13.0),
                                            );
                                        }
                                        if ai_busy && ui.small_button("取消").clicked() {
                                            status_cancel = true;
                                        }
                                        if can_copy && ui.small_button("复制译文").clicked() {
                                            copy_translated = true;
                                        }
                                    });
                                });
                        });
                }
            }
            // PS 式内联文本编辑：文本框内直接出现闪动光标，输入即所见
            if editor.is_editing_text() && mode == Mode::Edit {
                let ppp = ui.ctx().pixels_per_point();
                let edit_rect = editor.text_edit_state().unwrap().rect;
                let anchor = egui::pos2(edit_rect.x as f32 / ppp, edit_rect.y as f32 / ppp);
                let size = egui::vec2(edit_rect.width as f32 / ppp, edit_rect.height as f32 / ppp);
                let col = egui::Color32::from_rgba_unmultiplied(editor.stroke_color().r, editor.stroke_color().g, editor.stroke_color().b, editor.stroke_color().a);
                // 输入框按草稿样式渲染（字号/字体实时所见即所得；加粗混合策略，
                // 2026-09-10 用户定稿：有粗体变体用真粗体 family，无变体用阴影垫底）
                let font_size_val = editor.editing_text_size().unwrap_or_else(|| editor.text_font_size());
                // 按编辑中文字（或悬停预览中）的独立字体渲染内联输入框
                let edit_font = editor.editing_text_font();
                let edit_bold = editor.editing_text_bold();
                let edit_bold_variant =
                    edit_bold && crate::ui::gui::bold_variant_available(edit_font.as_deref());
                let fam = if edit_bold_variant {
                    crate::ui::gui::ensure_annotation_family_bold(ui.ctx(), edit_font.as_deref())
                } else {
                    crate::ui::gui::ensure_annotation_family(ui.ctx(), edit_font.as_deref())
                };
                let font_id = egui::FontId::new(font_size_val / ppp, fam);
                let bold_shadow = egui::Color32::from_rgba_unmultiplied(
                    editor.stroke_color().r,
                    editor.stroke_color().g,
                    editor.stroke_color().b,
                    (editor.stroke_color().a as f32 * 0.9) as u8,
                );
                egui::Area::new(egui::Id::new("text_inline_edit"))
                    .fixed_pos(anchor)
                    .order(egui::Order::Foreground)
                    .show(ui.ctx(), |ui| {
                        ui.set_min_size(size);
                        ui.set_max_size(size);
                        egui::Frame::new()
                            .fill(egui::Color32::TRANSPARENT)
                            .corner_radius(2.0)
                            .inner_margin(egui::Margin::symmetric(2, 0))
                            .show(ui, |ui| {
                                ui.visuals_mut().override_text_color = Some(col);
                                if let Some(state) = editor.text_edit_state_mut() {
                                    // `show`（而非 `ui.add`）取回排好的 galley 与精确起点，
                                    // 无变体垫底阴影与输入文字同 galley、对齐分毫不差
                                    let out = egui::TextEdit::multiline(&mut state.buffer)
                                        .hint_text("输入文字…")
                                        .font(font_id.clone())
                                        .frame(egui::Frame::new().fill(egui::Color32::TRANSPARENT))
                                        .desired_width(size.x - 4.0)
                                        .desired_rows(((size.y / (font_size_val / ppp * 1.25)).ceil() as usize).max(1))
                                        .show(ui);
                                    // 无变体垫底：复用 TextEdit 刚排好的 galley 按预览
                                    // 同款三向偏移叠阴影（同字形同换行，只有 0.8pt
                                    // 位移是刻意的加粗，没有重影）。
                                    if edit_bold && !edit_bold_variant {
                                        for (dx, dy) in [(0.8, 0.0), (0.0, 0.8), (0.8, 0.8)] {
                                            ui.painter().galley(
                                                out.galley_pos + egui::vec2(dx, dy),
                                                out.galley.clone(),
                                                bold_shadow,
                                            );
                                        }
                                    }
                                    let needs_focus = ui.ctx().memory(|m| m.focused() != Some(out.response.id));
                                    if needs_focus {
                                        ui.ctx().memory_mut(|m| m.request_focus(out.response.id));
                                    }
                                    // 富文本选区回填：工具条颜色/加粗/字体在有选区时只 splice
                                    // 选区（见 `Editor::apply_text_color/bold/end_text_font_hover`），
                                    // 无选区走整框 legacy。`cursor_range` 字符下标（非字节），
                                    // 与草稿 `char_styles` 的字符对齐一致。
                                    let sel = out.cursor_range.and_then(|rg| {
                                        let a: usize = rg.primary.index.0;
                                        let b: usize = rg.secondary.index.0;
                                        if a == b { None } else { Some((a.min(b), a.max(b))) }
                                    });
                                    editor.note_edit_frame(sel);
                                    edit_text_id = Some(out.response.id);
                                }
                            });
                    });
            }
        });
        // 缓存工具条实际渲染矩形；首帧测量到边界后请求再绘一帧，
        // 让贴合的遮罩挖洞立即生效
        self.font_picker = font_picker_state;
        self.edit_text_id = edit_text_id;
        let first_measure = self.bar_rect_cache.is_none() && bar_actual.is_some();
        self.bar_rect_cache = bar_actual;
        if first_measure {
            window.request_redraw();
        }
        if let Some(a) = action {
            self.handle_toolbar_action(a);
        }
        if status_cancel {
            self.cancel_ai();
        }
        if copy_translated {
            self.copy_translated_text();
        }
        presented
    }

    /// 复制全部译文到剪贴板（译文只渲染成像素、不可选中，此按钮是复制出口）。
    ///
    /// 成功后状态行提示"已复制译文"（译文覆盖保留，可继续保存）；
    /// 失败只给状态行，不退出。
    fn copy_translated_text(&mut self) {
        let parts: Vec<&str> = self
            .ai_translated
            .iter()
            .map(|r| r.translated.as_str())
            .filter(|t| !t.trim().is_empty())
            .collect();
        if parts.is_empty() {
            return;
        }
        match clipboard::copy_text(&parts.join("\n")) {
            Ok(()) => self.ai_status = Some(String::from("已复制译文")),
            Err(e) => self.ai_status = Some(format!("复制失败：{e:#}")),
        }
        self.window.request_redraw();
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
                // 纯色遮挡共享统一颜色：若当前为纯色样式，同步更新
                if matches!(self.editor.mosaic_style(), crate::annotation::MosaicStyle::Solid { .. }) {
                    self.editor.set_mosaic_style(crate::annotation::MosaicStyle::Solid { color });
                }
                self.window.request_redraw();
            }
            ToolbarAction::SetStrokeWidth(width) => {
                self.editor.set_stroke_width(width);
                self.window.request_redraw();
            }
            ToolbarAction::SetTextFontSize(size) => {
                // 有选中文字 → 实时改选中标注；无选中 → 新标注默认值（apply_* 内部分流）
                self.editor.apply_text_font_size(size);
                self.window.request_redraw();
            }
            ToolbarAction::SetTextColorAt(color) => {
                self.editor.apply_text_color(color);
                self.restore_edit_focus();
                self.window.request_redraw();
            }
            ToolbarAction::SetTextFont(font) => {
                // 提交选中文字字体（restore 由 end_text_font_hover 统一处理）
                self.editor.end_text_font_hover(Some(font.clone()));
                // 同步为全局默认（2026-09-10 用户定稿）：后续新文字默认用它，
                // 设置菜单下次打开显示它。立即先生效（fontsel 全局），写盘与
                // App.config 热更新由宿主经 take_pending_config 完成。
                crate::utils::fontsel::set_annotation_font(font.clone());
                let mut cfg = (*self.config).clone();
                cfg.ui.annotation_font = font.unwrap_or_default();
                self.pending_config = Some(cfg);
                self.restore_edit_focus();
                self.window.request_redraw();
            }
            ToolbarAction::TextFontHover(font) => {
                match font {
                    Some(path) => self.editor.hover_text_font(Some(path)),
                    None => self.editor.end_text_font_hover(None),
                }
                self.window.request_redraw();
            }
            ToolbarAction::SetTextBold(bold) => {
                self.editor.apply_text_bold(bold);
                self.restore_edit_focus();
                self.window.request_redraw();
            }
            ToolbarAction::SetMosaicStyle(style) => {
                self.editor.set_mosaic_style(style.clone());
                if let crate::annotation::MosaicStyle::Solid { color } = &style {
                    self.editor.set_stroke_color(*color);
                }
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
            ToolbarAction::ExtractText => self.start_extract(),
            ToolbarAction::Translate => self.start_translate(),
        }
    }

    /// 工具条提交样式后，把焦点和选区还给内联输入框（外援 R3）。
    ///
    /// 点加粗/颜色/字体按钮当帧 TextEdit 失焦（选区被折叠，提交靠粘性选区），
    /// 若不恢复焦点，用户点一次按钮输入框就丢光标、体验别扭。选区按
    /// `edit_sel_range`（当帧→粘性）重建后写回 egui（`load/store`），无选区
    /// 时只恢复焦点。非编辑态直接返回。
    fn restore_edit_focus(&self) {
        if !self.editor.is_editing_text() {
            return;
        }
        let Some(id) = self.edit_text_id else { return };
        let ctx = self.gui.egui_ctx();
        if let Some((s, e)) = self.editor.edit_sel_range() {
            if let Some(mut st) = egui::text_edit::TextEditState::load(ctx, id) {
                let rg = egui::text::CCursorRange::two(
                    egui::text::CCursor::new(s),
                    egui::text::CCursor::new(e),
                );
                st.cursor.set_char_range(Some(rg));
                st.store(ctx, id);
            }
        }
        ctx.memory_mut(|m| m.request_focus(id));
    }

    /// 注入 AI 完成回传（宿主在打开覆盖层时设置，接到 winit `EventLoopProxy`）。
    pub fn set_ai_notify(&mut self, notify: impl Fn(AiDone) + Send + Sync + 'static) {
        self.ai_notify = Some(std::sync::Arc::new(notify));
    }

    /// AI 后台任务完成（宿主经 `UserEvent` 转交；过期请求直接丢弃）。
    pub fn on_ai_done(&mut self, done: AiDone) {
        match done {
            AiDone::Ocr { req_id, regions } => {
                if req_id != self.ai_req {
                    return;
                }
                match regions {
                    Ok(rs) => {
                        // 坐标链路诊断（框错位时看日志即知哪段换算错）
                        tracing::info!(
                            "OCR 完成：{} 区域，选区 {:?}，首框 {:?}",
                            rs.len(),
                            self.selection,
                            rs.first().map(|r| r.bbox)
                        );
                        self.ai_cached_sel = self.selection;
                        self.ai_cached_regions = rs;
                        if self.ai_job == AiJob::Translate {
                            self.spawn_translate();
                        } else {
                            self.ai_busy = false;
                            self.ai_start = None;
                            self.ai_job = AiJob::Idle;
                            self.finish_extract();
                        }
                    }
                    Err(e) => {
                        self.ai_busy = false;
                        self.ai_start = None;
                        self.ai_job = AiJob::Idle;
                        self.ai_status = Some(format!("识别失败：{e}"));
                        tracing::warn!("OCR 失败: {e}");
                    }
                }
            }
            AiDone::Translate { req_id, regions } => {
                if req_id != self.ai_req {
                    return;
                }
                self.ai_busy = false;
                self.ai_start = None;
                self.ai_job = AiJob::Idle;
                match regions {
                    Ok(rs) => {
                        let n = rs.len();
                        tracing::info!(
                            "翻译完成：{} 区域，覆盖框 {:?}",
                            n,
                            rs.iter().map(|r| r.bbox).collect::<Vec<_>>()
                        );
                        self.ai_translated = rs;
                        // 成功不提示（状态行会挡译文）；只提示空结果与失败
                        self.ai_status = if n == 0 {
                            Some(String::from("翻译结果为空，未覆盖"))
                        } else {
                            None
                        };
                    }
                    Err(e) => {
                        self.ai_status = Some(format!("翻译失败：{e}"));
                        tracing::warn!("翻译失败: {e}");
                    }
                }
            }
        }
        self.window.request_redraw();
    }

    /// 选区变化时清掉 AI 态（译文覆盖会对不齐、缓存不可复用，在途结果作废）。
    fn invalidate_ai(&mut self) {
        self.ai_req += 1;
        self.ai_job = AiJob::Idle;
        self.ai_busy = false;
        self.ai_start = None;
        self.ai_status = None;
        self.ai_translated.clear();
        self.ai_cached_sel = None;
        self.ai_cached_regions.clear();
    }

    /// 取消在途 AI 任务（序号自增使迟到结果作废；OCR 缓存保留，
    /// 选区未变时重试直接命中，无需重新识别）。
    fn cancel_ai(&mut self) {
        self.ai_req += 1;
        self.ai_job = AiJob::Idle;
        self.ai_busy = false;
        self.ai_start = None;
        self.ai_status = None;
        self.window.request_redraw();
    }

    /// 当前选区命中的 OCR 缓存（选区一致才复用，避免重复识别）。
    fn cached_regions(&self) -> Option<Vec<TextRegion>> {
        match (self.selection, self.ai_cached_sel) {
            (Some(a), Some(b)) if a == b => Some(self.ai_cached_regions.clone()),
            _ => None,
        }
    }

    /// 选区裁剪图（sRGB，供 OCR/翻译采样与裁剪用）。
    fn selection_dyn_image(&self, sel: &Rect) -> image::DynamicImage {
        image::DynamicImage::ImageRgba8(
            image::imageops::crop_imm(
                self.image.as_ref(),
                sel.x as u32,
                sel.y as u32,
                sel.width,
                sel.height,
            )
            .to_image(),
        )
    }

    /// 起 OCR 后台任务（调用方已填好 `ai_job` 路由与状态行）。
    fn spawn_ocr(&mut self, sel: &Rect, status: &str) {
        let Some(notify) = self.ai_notify.clone() else {
            self.ai_busy = false;
            self.ai_job = AiJob::Idle;
            self.ai_status = Some(String::from("AI 链路未就绪"));
            return;
        };
        self.ai_req += 1;
        self.ai_busy = true;
        self.ai_start = Some(Instant::now());
        self.ai_status = Some(status.to_string());
        ai::spawn_ocr_job(
            self.selection_dyn_image(sel),
            (sel.x, sel.y),
            self.config.ocr.engine,
            self.ai_req,
            move |done| notify(done),
        );
    }

    /// 工具条「提取文字」：OCR 完成后直接复制到剪贴板（不弹面板），
    /// 缓存命中直接复制，否则起 OCR。
    fn start_extract(&mut self) {
        let Some(sel) = self.selection else { return };
        self.ai_job = AiJob::Extract;
        self.ai_status = None;
        if let Some(rs) = self.cached_regions() {
            self.ai_cached_regions = rs;
            self.finish_extract();
        } else {
            self.spawn_ocr(&sel, "正在识别文字…");
        }
        self.window.request_redraw();
    }

    /// OCR 区域拼成文本并直接复制到剪贴板；成功后自动退出截图页
    /// （空结果/复制失败只给状态行，不退出）。
    fn finish_extract(&mut self) {
        let mut parts = Vec::new();
        for b in merge_regions_into_blocks(self.ai_cached_regions.clone()) {
            if let Some(t) = b.merged_text.as_ref() {
                if !t.trim().is_empty() {
                    parts.push(t.clone());
                }
            }
        }
        if parts.is_empty() {
            self.ai_status = Some(String::from("未识别到文字"));
            self.window.request_redraw();
        } else {
            let text = parts.join("\n\n");
            match clipboard::copy_text(&text) {
                // 复制成功直接退出截图页（用户不再需要手动 Esc）
                Ok(()) => self.exit_requested = true,
                Err(e) => {
                    self.ai_status = Some(format!("复制失败：{e:#}"));
                    self.window.request_redraw();
                }
            }
        }
    }

    /// 工具条「翻译」：缓存命中直接进管线，否则先 OCR、完成后自动链式翻译。
    fn start_translate(&mut self) {
        let Some(sel) = self.selection else { return };
        self.ai_job = AiJob::Translate;
        self.ai_status = None;
        if self.cached_regions().is_some() {
            self.spawn_translate();
        } else {
            self.spawn_ocr(&sel, "正在识别文字（稍后自动翻译）…");
        }
        self.window.request_redraw();
    }

    /// 组装语义块并起翻译后台任务（空文本块全部跳过，无内容只给状态行）。
    fn spawn_translate(&mut self) {
        let Some(sel) = self.selection else { return };
        let blocks: Vec<_> =
            merge_regions_into_blocks(self.ai_cached_regions.clone())
                .into_iter()
                .filter(|b| {
                    b.merged_text.as_ref().is_some_and(|t| !t.trim().is_empty())
                })
                .collect();
        if blocks.is_empty() {
            self.ai_busy = false;
            self.ai_start = None;
            self.ai_job = AiJob::Idle;
            self.ai_status = Some(String::from("未识别到文字，无需翻译"));
            return;
        }
        let Some(notify) = self.ai_notify.clone() else {
            self.ai_busy = false;
            self.ai_start = None;
            self.ai_job = AiJob::Idle;
            self.ai_status = Some(String::from("AI 链路未就绪"));
            return;
        };
        self.ai_req += 1;
        self.ai_busy = true;
        self.ai_start = Some(Instant::now());
        self.ai_status = Some(String::from("正在翻译…"));
        ai::spawn_translate_job(
            self.selection_dyn_image(&sel),
            blocks,
            (sel.x, sel.y),
            self.config.translate.clone(),
            self.ai_req,
            move |done| notify(done),
        );
    }

    /// 译文覆盖预览（egui 层：背景色块 + 译文，导出走 CPU 重绘见 [`Self::crop_selection`]）。
    ///
    /// 坐标与选区同一约定：全图物理像素 ÷ ppp（见 [`to_pts`]）；排版吃
    /// [`render::fit_font_size_box`] 的宽高双约束结果（与导出同函数），
    /// 译文绝不溢出框（先前只按宽度缩，行高超框是实机溢出的根因）。
    fn draw_translated_preview(
        painter: &egui::Painter,
        ppp: f32,
        regions: &[TranslatedRegion],
    ) {
        for r in regions {
            let rect = egui::Rect::from_min_max(
                egui::pos2(r.bbox.x as f32 / ppp, r.bbox.y as f32 / ppp),
                egui::pos2(
                    (r.bbox.x + r.bbox.width) as f32 / ppp,
                    (r.bbox.y + r.bbox.height) as f32 / ppp,
                ),
            );
            if rect.width() < 2.0 || rect.height() < 2.0 {
                continue;
            }
            let bg =
                egui::Color32::from_rgb(r.bg_color[0], r.bg_color[1], r.bg_color[2]);
            let fg = egui::Color32::from_rgb(
                r.text_color[0],
                r.text_color[1],
                r.text_color[2],
            );
            painter.rect_filled(rect, 0.0, bg);
            // 与导出同排版：物理像素下宽高双约束 + 同字体链路换行，再换算回 pt 绘制。
            // egui 字体度量与 ab_glyph 不完全一致（尤其拉丁字符），行宽可能超框，
            // 故文字层按背景矩形裁剪——译文绝不画到框外（导出侧见临时图钳制）。
            let clipped = painter.with_clip_rect(rect);
            let avail_px = rect.width() * ppp - 4.0;
            let avail_py = rect.height() * ppp - 4.0;
            let (size_px, lines) = render::fit_font_size_box(
                r.est_font_size as f32,
                &r.translated,
                avail_px,
                avail_py,
                false,
            );
            let size = size_px / ppp;
            let mut y = rect.min.y + 2.0;
            for line in lines {
                if y > rect.max.y {
                    break;
                }
                clipped.text(
                    egui::pos2(rect.min.x + 2.0, y),
                    egui::Align2::LEFT_TOP,
                    line,
                    crate::ui::gui::annotation_font_id(size),
                    fg,
                );
                y += size * 1.25;
            }
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
                draw_size_label(&painter, &sel, sel_pts, screen_rect);
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
                draw_size_label(&painter, &sel, sel_pts, screen_rect);
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

/// 选区尺寸标签（选区外侧：优先放上沿外，顶部没地儿放下沿外；
/// 之前放选区左上内侧，会盖住译文）。
fn draw_size_label(
    painter: &egui::Painter,
    sel: &Rect,
    sel_pts: egui::Rect,
    screen_pts: egui::Rect,
) {
    let text = format!("{} x {}", sel.width, sel.height);
    let galley = painter.layout_no_wrap(
        text,
        egui::FontId::proportional(13.0),
        egui::Color32::WHITE,
    );
    let h = galley.size().y + 4.0;
    // 上沿外优先（标签底贴选区顶 - 4）；不够高则下沿外
    let mut y = sel_pts.min.y - 4.0 - h;
    if y < screen_pts.min.y {
        y = sel_pts.max.y + 4.0;
    }
    let mut x = sel_pts.min.x + 6.0;
    x = x.clamp(
        screen_pts.min.x,
        (screen_pts.max.x - galley.size().x - 8.0).max(screen_pts.min.x),
    );
    let pos = egui::pos2(x, y + 2.0);
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
