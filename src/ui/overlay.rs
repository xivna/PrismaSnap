//! 全屏选区覆盖层窗口（仅 Windows 平台编译）。
//!
//! 单窗口切换架构（Snipaste 式，见 PROGRESS.md 决策记录）：
//! 同一窗口内 `Selecting`（截图 + 半透明遮罩 + 拖动选区）→
//! `Preview`（遮罩加深 + 选区高亮 + 确认提示条）两种模式切换，
//! 不做销毁重建。选区确认后 Esc 取消、Enter / Ctrl+C 复制、Ctrl+S 保存。
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

use crate::config::{Config, SaveFormat};
use crate::utils::math::Rect;
use crate::utils::{clipboard, image_codec, paths, time};

use super::gui::GuiState;

/// 覆盖层模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// 选择中：截图 + 半透明遮罩，等待拖动选区。
    Selecting,
    /// 已选定：遮罩加深、选区高亮，等待确认（复制/保存/取消）。
    Preview,
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
            current_cursor: None,
            drag_start: None,
            selection: None,
            monitor_rect: shot.monitor_rect,
            modifiers: ModifiersState::empty(),
            config,
            exit_requested: false,
        })
    }

    /// 窗口 id（宿主按 id 分发事件）。
    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    /// 事件入口：先喂 egui 记录输入，再处理业务逻辑与重绘。
    pub fn on_window_event(&mut self, event: &WindowEvent) {
        self.gui.on_window_event(self.window.as_ref(), event);
        match event {
            WindowEvent::CloseRequested => self.exit_requested = true,
            WindowEvent::Resized(size) => self.gui.resize(size.width, size.height),
            WindowEvent::ModifiersChanged(state) => self.modifiers = state.state(),
            WindowEvent::CursorMoved { position, .. } => {
                self.current_cursor = Some((position.x as f32, position.y as f32));
                // 拖动中实时更新选区
                if self.drag_start.is_some() {
                    self.update_selection_from_drag();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => self.on_press(),
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

    /// 按下左键：`Preview` 模式重置选区回到 `Selecting`；否则开始拖动。
    fn on_press(&mut self) {
        if self.mode == Mode::Preview {
            self.mode = Mode::Selecting;
            self.selection = None;
        }
        self.drag_start = self.current_cursor;
        self.window.request_redraw();
    }

    /// 释放左键：选区有效则进入 `Preview`，否则视为无效拖动清空。
    fn on_release(&mut self) {
        self.drag_start = None;
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

    /// 键盘动作：Esc 取消、Enter 复制、Ctrl+C 复制、Ctrl+S 保存。
    fn on_key(&mut self, key: &KeyEvent) {
        if key.state != ElementState::Pressed {
            return;
        }
        match key.physical_key {
            PhysicalKey::Code(KeyCode::Escape) => {
                self.exit_requested = true;
            }
            PhysicalKey::Code(KeyCode::Enter) => {
                if self.mode == Mode::Preview {
                    self.copy_and_exit();
                }
            }
            PhysicalKey::Code(KeyCode::KeyC) if self.modifiers.control_key() => {
                if self.mode == Mode::Preview {
                    self.copy_and_exit();
                }
            }
            PhysicalKey::Code(KeyCode::KeyS)
                if self.modifiers.control_key() && self.mode == Mode::Preview =>
            {
                self.save_and_exit();
            }
            _ => {}
        }
    }

    /// 裁剪选区图像（物理坐标 = 图像像素坐标，直接裁剪）。
    fn crop_selection(&self) -> Option<image::RgbaImage> {
        let sel = self.selection?;
        Some(
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
    fn save_and_exit(&mut self) {
        match self.crop_selection() {
            Some(img) => match save_shot(&img, &self.config) {
                Ok(path) => tracing::info!("已保存选区: {}", path.display()),
                Err(e) => tracing::error!("保存失败: {e:#}"),
            },
            None => tracing::warn!("保存请求但选区为空"),
        }
        self.exit_requested = true;
    }

    /// 渲染一帧：截图 + 遮罩 + 选区 + 提示条。
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
        self.gui.render(self.window.as_ref(), move |ui| {
            draw_frame(ui, texture_id, hdr_mode, img_size, mode, selection, monitor_rect);
        });
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
/// 注意：egui 默认字体不含 CJK，提示文字暂用英文（中文字体嵌入见 Phase 3）。
///
/// HDR 模式下截图不在这里绘制（由合成 pass 直通显示），egui 只画遮罩/选区/提示层。
fn draw_frame(
    ui: &mut egui::Ui,
    texture_id: Option<egui::TextureId>,
    hdr_mode: bool,
    img_size: (u32, u32),
    mode: Mode,
    selection: Option<Rect>,
    monitor_rect: Rect,
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
                dim_outside_selection(&painter, sel_pts, screen_rect, 110);
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
                    "Drag to select area   (Esc to cancel)",
                    egui::FontId::proportional(18.0),
                    egui::Color32::WHITE,
                );
            }
        },
        Mode::Preview => {
            if let Some(sel) = selection {
                let sel_pts = to_pts(&sel, ppp);
                // 更深遮罩突出选区（挖洞保留选区亮度）
                dim_outside_selection(&painter, sel_pts, screen_rect, 170);
                painter.rect_stroke(
                    sel_pts,
                    0.0,
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(0, 140, 255)),
                    egui::StrokeKind::Outside,
                );
                draw_size_label(&painter, &sel, sel_pts);
                draw_action_bar(&painter, sel_pts, screen_rect);
            }
        }
    }
}

/// 选区外四块矩形半透明遮罩（egui painter 无挖洞原语，用四块拼）。
fn dim_outside_selection(
    painter: &egui::Painter,
    sel_pts: egui::Rect,
    screen_rect: egui::Rect,
    alpha: u8,
) {
    let dark = egui::Color32::from_black_alpha(alpha);
    painter.rect_filled(
        egui::Rect::from_min_max(screen_rect.min, egui::pos2(screen_rect.max.x, sel_pts.min.y)),
        0.0,
        dark,
    );
    painter.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(screen_rect.min.x, sel_pts.min.y),
            egui::pos2(sel_pts.min.x, sel_pts.max.y),
        ),
        0.0,
        dark,
    );
    painter.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(sel_pts.max.x, sel_pts.min.y),
            egui::pos2(screen_rect.max.x, sel_pts.max.y),
        ),
        0.0,
        dark,
    );
    painter.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(screen_rect.min.x, sel_pts.max.y),
            screen_rect.max,
        ),
        0.0,
        dark,
    );
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

/// 预览模式动作提示条（选区下方居中，贴底时翻到上方）。
fn draw_action_bar(painter: &egui::Painter, sel_pts: egui::Rect, screen_rect: egui::Rect) {
    let text = "Enter / Ctrl+C Copy    Ctrl+S Save    Esc Cancel".to_owned();
    let galley = painter.layout_no_wrap(
        text,
        egui::FontId::proportional(14.0),
        egui::Color32::WHITE,
    );
    let pad = egui::vec2(14.0, 8.0);
    let bar_size = galley.size() + pad * 2.0;
    let gap = 10.0;
    // 优先放选区下方，空间不足放上方
    let bar_min_y = if sel_pts.max.y + gap + bar_size.y <= screen_rect.max.y {
        sel_pts.max.y + gap
    } else {
        (sel_pts.min.y - gap - bar_size.y).max(screen_rect.min.y)
    };
    let bar_rect = egui::Rect::from_min_size(
        egui::pos2(
            (sel_pts.center().x - bar_size.x / 2.0).clamp(
                screen_rect.min.x + 4.0,
                screen_rect.max.x - bar_size.x - 4.0,
            ),
            bar_min_y,
        ),
        bar_size,
    );
    painter.rect_filled(bar_rect, 6.0, egui::Color32::from_black_alpha(200));
    painter.rect_stroke(
        bar_rect,
        6.0,
        egui::Stroke::new(1.0, egui::Color32::from_white_alpha(40)),
        egui::StrokeKind::Outside,
    );
    painter.galley(bar_rect.min + pad, galley, egui::Color32::WHITE);
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

/// 按配置把截图保存到文件，返回保存路径。
fn save_shot(img: &image::RgbaImage, config: &Config) -> anyhow::Result<std::path::PathBuf> {
    let dir = resolve_save_dir(&config.save.dir);
    std::fs::create_dir_all(&dir)?;
    let name = format!(
        "PrismaSnap_{}.{}",
        time::timestamp_str(),
        match config.save.format {
            SaveFormat::Png => "png",
            SaveFormat::Jpeg => "jpg",
        }
    );
    let path = dir.join(name);
    match config.save.format {
        SaveFormat::Png => image_codec::save_png(img, &path)?,
        SaveFormat::Jpeg => image_codec::save_jpeg(img, &path, config.save.jpeg_quality)?,
    }
    Ok(path)
}
