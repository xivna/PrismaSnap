//! 设置主界面（仅 Windows 平台编译）。
//!
//! 主窗口：热键录制改绑、保存行为（含"始终询问位置"模式）、捕获选项
//! （光标/HDR 降级）、LLM API 配置（OpenAI 兼容，Phase 4 直接消费）。
//!
//! 交互模型：
//! - **实时保存**：任何控件变更即置位 `pending_save`，宿主（`main.rs`）立即写盘
//!   并热更新 `App.config`（热键仅在变更时 `rebind`）；无「保存/取消」按钮。
//! - **热键录制**：点「录制」进入录制态，捕获下一次「修饰键 + 主键」组合
//!   （Esc 取消），生成 `"Ctrl+Alt+A"` 风格字符串；不做文本输入。
//!
//! 渲染模型：`GuiState::render` 的闭包签名是 `FnMut(&mut Ui)`，若直接闭包内借
//! `&mut self.draft` 会与 `&mut self.gui` 冲突，故每帧先把 `draft`/`dir_input`
//! clone 到局部变量，闭包内编辑局部变量、结束写回（`Config` 极小，成本可忽略）。
//!
//! 中文字体：`GuiState::new` 会加载 Windows 系统字体（微软雅黑/黑体）作为
//! egui fallback 字体，界面可直接使用中文。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};

use anyhow::Context;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::config::{
    Config, LogLevel, MultimodalMode, OcrEngineKind, SaveFormat, SaveMode, Theme,
    TranslateMode, TranslatePrompts, DEFAULT_LLM_PARAMS_JSON,
};
use crate::ocr::create_engine;
use crate::ocr::download::{
    self, DlEvent, FileState, JobControl, MODELSCOPE_PAGE_URL, ORT_RELEASES_URL,
    RAPIDOCR_REPO_URL,
};
use crate::ocr::ocr_plugin_dir;

use super::gui::{palette, GuiState, Palette};

/// 设置窗口默认大小（逻辑像素）。
const DEFAULT_SIZE: (u32, u32) = (740, 560);

/// 设置窗口最小大小（逻辑像素）。
const MIN_SIZE: (u32, u32) = (640, 480);

/// 设置分区（左侧栏导航）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    /// 通用（截图热键）。
    General,
    /// 保存行为。
    Save,
    /// 捕获选项。
    Capture,
    /// 外观（主题）。
    Appearance,
    /// AI 接口。
    Ai,
}

impl Section {
    /// 侧栏导航顺序。
    const ALL: [Section; 5] = [
        Section::General,
        Section::Save,
        Section::Capture,
        Section::Appearance,
        Section::Ai,
    ];

    /// 侧栏显示名。
    fn label(self) -> &'static str {
        match self {
            Section::General => "通用",
            Section::Save => "保存",
            Section::Capture => "捕获",
            Section::Appearance => "外观",
            Section::Ai => "AI 接口",
        }
    }

    /// 内容区大标题。
    fn title(self) -> &'static str {
        match self {
            Section::Ai => "AI 接口（OpenAI 兼容）",
            other => other.label(),
        }
    }
}

/// 设置主界面窗口。
pub struct Settings {
    window: Arc<Window>,
    gui: GuiState,
    /// 编辑缓冲（打开时从当前配置克隆，保存成功前不改动正式配置）。
    draft: Config,
    /// 保存目录的字符串编辑缓冲（`PathBuf` 不便直接进文本框）。
    dir_input: String,
    /// 当前左侧栏选中的分区。
    active_section: Section,
    /// 是否处于热键录制状态（等待用户按下组合键）。
    recording_hotkey: bool,
    /// 当前修饰键状态（录制热键时组合修饰键）。
    modifiers: ModifiersState,
    /// 有变更待保存（宿主消费后清零）。
    pub pending_save: bool,
    /// 用户关闭窗口（宿主消费后清零）。
    pub close_requested: bool,
    /// 状态栏消息 `(是否成功, 文本)`，`None` 表示无提示。
    status: Option<(bool, String)>,
    /// OCR 插件模型下载会话状态（不进配置，关闭设置窗口即丢弃；下载线程
    /// 的 `JoinHandle` 不保留——线程结束即退出，结束事件经 channel 回收）。
    model_dl: ModelDownloadUi,
    /// 字体选择弹层状态（key = "interface"/"annotation"；纹理缓存与 ctx 绑定）。
    font_pickers: std::collections::HashMap<String, super::font_list::FontPickerState>,
    /// 大模型参数 JSON 输入框会话缓冲（`None` = 下帧从配置重载；不进配置，
    /// 关闭设置窗口即丢弃。编辑中非法只提示不保存，失焦时合法美化落盘、
    /// 非法恢复上一版）。
    params_ed: ParamsEditUi,
}

/// 大模型参数区编辑状态（设置页"大模型参数"卡片用）。
#[derive(Default)]
struct ParamsEditUi {
    /// 输入框文本缓冲（`None` = 下帧从 `draft.translate.params_json` 重载）。
    buf: Option<String>,
    /// 当前提示（编辑中非法错误 / 失焦恢复通知），`err_until` 过期即消失。
    err: Option<String>,
    /// 提示可见截止时间（3 秒自动消失用）。
    err_until: Option<std::time::Instant>,
    /// 上帧输入框是否有焦点（失焦边沿检测用）。
    had_focus: bool,
}

impl ParamsEditUi {
    /// 3 秒提示是否还可见（可见时宿主需持续重绘以保证按时消失）。
    fn notice_visible(&self) -> bool {
        match (self.err.as_ref(), self.err_until) {
            (Some(_), Some(until)) => std::time::Instant::now() < until,
            _ => false,
        }
    }
}

/// 单文件下载任务的 UI 侧句柄（`None` 表示该行无在途/暂停任务）。
struct FileJob {
    /// 事件接收端（线程结束/暂停即失效，由 `poll` 回收）。
    rx: Receiver<DlEvent>,
    /// 暂停/取消开关（按钮写，线程读）。
    ctl: Arc<JobControl>,
    /// 最新进度 `(已下字节, 总字节)`。
    progress: Option<(u64, Option<u64>)>,
    /// 线程是否还在跑（`false` = 已暂停，线程已退出、`.part` 保留）。
    live: bool,
}

/// OCR 插件模型下载的会话状态（设置页 AI 分区"插件模型"卡片用）。
#[derive(Default)]
struct ModelDownloadUi {
    /// 是否展开手动下载帮助窗口。
    show_help: bool,
    /// 四文件任务槽（下标与 `download::model_files()` 一致）。
    jobs: [Option<FileJob>; 4],
    /// 全局结果消息 `(是否成功, 文本)`（失败含手动下载指引）。
    result: Option<(bool, String)>,
    /// 帮助窗口内的复制反馈（刚复制的 URL）。
    copied: Option<String>,
}


impl ModelDownloadUi {
    /// 是否有在跑的下载线程（进度动画用）。
    fn any_live(&self) -> bool {
        self.jobs.iter().flatten().any(|j| j.live)
    }

    /// 是否有任务槽被占用（含暂停）。
    fn any_job(&self) -> bool {
        self.jobs.iter().any(|j| j.is_some())
    }

    /// 起某文件的后台下载线程（幂等：该行已有任务时直接返回）。
    fn start(&mut self, index: usize) {
        if index >= 4 || self.jobs[index].is_some() {
            return;
        }
        let dir = match ocr_plugin_dir() {
            Ok(d) => d,
            Err(e) => {
                self.result = Some((false, format!("插件目录解析失败：{e:#}")));
                return;
            }
        };
        let (tx, rx) = mpsc::channel();
        let ctl = Arc::new(JobControl::default());
        let ctl2 = ctl.clone();
        std::thread::spawn(move || download::run_file_job(dir, index, tx, ctl2));
        self.jobs[index] = Some(FileJob { rx, ctl, progress: None, live: true });
        self.result = None;
    }

    /// 暂停某行（线程刷盘退出、保留 `.part`；`Paused` 事件到后 `live=false`）。
    fn pause(&mut self, index: usize) {
        if let Some(job) = self.jobs[index].as_ref() {
            job.ctl.pause.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// 继续某行（暂停的线程已退出，重起一个按 `.part` 大小续传）。
    fn resume(&mut self, index: usize) {
        if !matches!(self.jobs[index].as_ref(), Some(job) if !job.live) {
            return;
        }
        self.jobs[index] = None;
        self.start(index);
    }

    /// 取消某行（在跑：置旗由线程删 `.part`；已暂停：直接删 `.part`；行回初始态）。
    fn cancel(&mut self, index: usize) {
        let live = matches!(self.jobs[index].as_ref(), Some(job) if job.live);
        if live {
            if let Some(job) = self.jobs[index].as_ref() {
                job.ctl.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            return;
        }
        // 已暂停（线程不在了）：直接删半截文件，行回到初始"下载"
        if self.jobs[index].is_some() {
            if let Ok(dir) = ocr_plugin_dir() {
                let part = dir.join(format!("{}.part", download::model_files()[index].local));
                let _ = std::fs::remove_file(&part);
                // DLL 的 zip 半截固定名，同样清理
                if index == 3 {
                    let _ = std::fs::remove_file(dir.join("ort_package.zip.part"));
                }
            }
            self.jobs[index] = None;
        }
    }

    /// 取消全部任务（标题行"取消"：在跑的置旗，已暂停的直接清 `.part`）。
    fn cancel_all(&mut self) {
        for i in 0..4 {
            self.cancel(i);
        }
    }

    /// 排空各任务事件（设置页每帧调用；`JobEnd` 到后回收任务槽）。
    fn poll(&mut self) {
        for i in 0..4 {
            let mut job_end: Option<(bool, String, bool)> = None;
            if let Some(job) = self.jobs[i].as_mut() {
                while let Ok(ev) = job.rx.try_recv() {
                    match ev {
                        DlEvent::Progress { done, total, .. } => {
                            job.progress = Some((done, total));
                        }
                        DlEvent::FileDone { .. } => {}
                        DlEvent::Paused { done, .. } => {
                            job.live = false;
                            job.progress = Some((done, None));
                        }
                        DlEvent::JobEnd { ok, msg, cancelled, .. } => {
                            job_end = Some((ok, msg, cancelled));
                        }
                    }
                }
            }
            if let Some((ok, msg, cancelled)) = job_end {
                self.jobs[i] = None;
                if cancelled {
                    continue;
                }
                if !ok {
                    self.result = Some((false, msg));
                    continue;
                }
                // 成功：四齐了才报全局完成（单文件完成由状态行变绿体现）
                if let Ok(dir) = ocr_plugin_dir() {
                    if download::all_ready(&dir) {
                        self.result = Some((true, String::from("全部下载完成，插件已就绪")));
                    }
                }
            }
        }
    }
}

impl Settings {
    /// 创建设置窗口（普通有边框、可调整大小）。
    ///
    /// `saved_pos` / `saved_size` 为上次关闭时的位置与大小（物理像素，见
    /// `UiConfig::settings_pos` / `settings_size`）：有值则恢复（位置按鼠标所在
    /// 显示器钳制，大小钳制到 [MIN_SIZE, 显示器]，防换屏后失联/窗口过大），
    /// 无值（首次打开）走默认尺寸与系统 placement。
    pub fn create_window(
        event_loop: &ActiveEventLoop,
        saved_pos: Option<(i32, i32)>,
        saved_size: Option<(u32, u32)>,
    ) -> anyhow::Result<Arc<Window>> {
        let mut attrs = Window::default_attributes()
            .with_title("PrismaSnap 设置")
            .with_inner_size(winit::dpi::LogicalSize::new(
                DEFAULT_SIZE.0 as f64,
                DEFAULT_SIZE.1 as f64,
            ))
            .with_min_inner_size(winit::dpi::LogicalSize::new(
                MIN_SIZE.0 as f64,
                MIN_SIZE.1 as f64,
            ))
            .with_resizable(true);
        if let Some((w, h)) = saved_size {
            let (_, _, mw, mh) = Self::monitor_rect_or_fallback();
            let w = w.clamp(MIN_SIZE.0, mw.max(MIN_SIZE.0 as i32) as u32);
            let h = h.clamp(MIN_SIZE.1, mh.max(MIN_SIZE.1 as i32) as u32);
            attrs = attrs.with_inner_size(winit::dpi::PhysicalSize::new(w, h));
        }
        if let Some((x, y)) = saved_pos {
            // 按鼠标所在显示器钳制：至少留 120×40 的标题栏可点，避免屏外失联
            let (mx, my, mw, mh) = Self::monitor_rect_or_fallback();
            let cx = x.clamp(mx, mx + mw.max(121) - 121);
            let cy = y.clamp(my, my + mh.max(41) - 41);
            attrs = attrs.with_position(winit::dpi::PhysicalPosition::new(cx, cy));
        }
        event_loop
            .create_window(attrs)
            .context("创建设置窗口失败")
            .map(Arc::new)
    }

    /// 上次关闭位置恢复用的显示器矩形（物理像素左上+宽高）。
    ///
    /// 取鼠标所在显示器（与截图目标一致）；查询失败回退 1920×1080，
    /// 调用方钳制逻辑不受影响（窗口最多偏一点，不会失联）。
    fn monitor_rect_or_fallback() -> (i32, i32, i32, i32) {
        match crate::capture::engine::monitor_rect_at_cursor() {
            Ok(r) => (r.x, r.y, r.width as i32, r.height as i32),
            Err(_) => (0, 0, 1920, 1080),
        }
    }

    /// 初始化设置窗口（克隆配置为编辑缓冲）。
    pub fn new(window: Arc<Window>, config: &Config) -> anyhow::Result<Self> {
        let gui = GuiState::new(&window, false)?;
        let dir_input = config.save.dir.to_string_lossy().into_owned();
        Ok(Self {
            window,
            gui,
            draft: config.clone(),
            dir_input,
            active_section: Section::General,
            recording_hotkey: false,
            modifiers: ModifiersState::empty(),
            pending_save: false,
            close_requested: false,
            status: None,
            model_dl: ModelDownloadUi::default(),
            font_pickers: std::collections::HashMap::new(),
            params_ed: ParamsEditUi::default(),
        })
    }

    /// 窗口 id（宿主按 id 分发事件）。
    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    /// 当前窗口位置（物理像素左上角），关闭时记录用；查询失败返回 `None`。
    pub fn outer_position(&self) -> Option<(i32, i32)> {
        self.window
            .outer_position()
            .map(|p| (p.x, p.y))
            .ok()
    }

    /// 当前窗口物理尺寸，关闭时记录用（下次打开恢复）。
    pub fn inner_size(&self) -> (u32, u32) {
        let s = self.window.inner_size();
        (s.width, s.height)
    }

    /// 聚焦已有窗口（托盘再次点「打开设置」时）。
    pub fn focus(&self) {
        self.window.focus_window();
        self.window.request_redraw();
    }

    /// 设置状态栏消息（保存成功/失败等）。
    pub fn set_status(&mut self, ok: bool, msg: impl Into<String>) {
        self.status = Some((ok, msg.into()));
        self.window.request_redraw();
    }

    /// 把目录/JSON 文本缓冲同步回编辑缓冲并返回其引用（保存前调用）。
    pub fn apply_draft(&mut self) -> &Config {
        self.draft.save.dir = PathBuf::from(self.dir_input.trim());
        // JSON 缓冲兜底：编辑中合法内容本就实时写入，这里再防一手极端时序
        // （如点 X 关闭那一帧无失焦帧）；非法内容忽略，保留上一版合法值。
        if let Some(buf) = self.params_ed.buf.as_deref() {
            settle_params_json(&mut self.draft, buf);
        }
        &self.draft
    }

    /// 热键注册失败时把编辑缓冲里的热键回退为当前生效值。
    pub fn revert_hotkey(&mut self, current: &str) {
        self.draft.hotkey = current.to_owned();
    }

    /// 是否处于热键录制状态（宿主据此挂起全局热键，避免拦截录制按键）。
    pub fn is_recording(&self) -> bool {
        self.recording_hotkey
    }

    /// 事件入口：先喂 egui，再处理业务逻辑与重绘。
    pub fn on_window_event(&mut self, event: &WindowEvent) {
        self.gui.on_window_event(self.window.as_ref(), event);
        match event {
            WindowEvent::CloseRequested => self.close_requested = true,
            WindowEvent::Resized(size) => self.gui.resize(size.width, size.height),
            WindowEvent::ModifiersChanged(state) => self.modifiers = state.state(),
            WindowEvent::KeyboardInput { event, .. } => {
                if self.recording_hotkey {
                    self.handle_recording_key(event);
                }
            }
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }

    /// 录制态下的键盘处理：Esc 取消，修饰键忽略，主键生成热键字符串。
    fn handle_recording_key(&mut self, key: &KeyEvent) {
        if key.state != ElementState::Pressed {
            return;
        }
        let PhysicalKey::Code(code) = key.physical_key else {
            return;
        };
        if code == KeyCode::Escape {
            self.recording_hotkey = false;
            self.window.request_redraw();
            return;
        }
        if is_modifier_code(code) {
            return;
        }
        let Some(key_str) = keycode_to_str(code) else {
            self.set_status(false, "不支持的按键，请换一个");
            return;
        };
        // 无修饰键的单键只允许功能键（F1-F24 等），避免误占普通字母/数字键
        if self.modifiers.is_empty() && !is_standalone_ok(key_str) {
            self.set_status(false, "请同时按下修饰键（Ctrl/Alt/Shift/Win）");
            return;
        }
        self.draft.hotkey = build_hotkey_str(self.modifiers, key_str);
        self.recording_hotkey = false;
        self.pending_save = true;
        self.window.request_redraw();
    }

    /// 渲染一帧设置界面。
    pub fn redraw(&mut self) {
        // 先排空模型下载线程事件（进度/结束），再进绘制闭包
        self.model_dl.poll();
        // clone 到局部变量，避免闭包与 self.gui 的双重可变借用
        // （model_dl 含 channel，用 mem::replace 整块搬出、用完搬回）
        let mut draft = self.draft.clone();
        let mut dir_input = self.dir_input.clone();
        let mut active_section = self.active_section;
        let mut changed = false;
        let mut start_recording = false;
        let recording = self.recording_hotkey;
        let status = self.status.clone();
        let mut mdl = std::mem::take(&mut self.model_dl);
        let mut font_pickers = std::mem::take(&mut self.font_pickers);
        let mut params_ed = std::mem::take(&mut self.params_ed);

        self.gui.render(self.window.as_ref(), |ui| {
            // 主题实时预览：改选项立即生效（正式写盘仍走 pending_save）
            super::gui::apply_theme(ui.ctx(), draft.ui.theme);
            draw_settings_ui(
                ui,
                &mut draft,
                &mut dir_input,
                &mut active_section,
                recording,
                status.as_ref(),
                &mut changed,
                &mut start_recording,
                &mut mdl,
                &mut font_pickers,
                &mut params_ed,
            );
        });

        // 闭包同步执行完毕，把编辑结果写回
        self.draft = draft;
        self.dir_input = dir_input;
        self.active_section = active_section;
        self.model_dl = mdl;
        self.font_pickers = font_pickers;
        self.params_ed = params_ed;
        if start_recording {
            self.recording_hotkey = true;
        }
        if changed {
            self.pending_save = true;
        }
        // 下载进行中时持续重绘，进度百分比才动；参数区 3 秒提示同理
        if self.model_dl.any_live() || self.params_ed.notice_visible() {
            self.window.request_redraw();
        }
    }
}

/// 左侧栏宽度（逻辑点）。
const SIDEBAR_WIDTH: f32 = 188.0;

/// 内容区左右留白。
const CONTENT_MARGIN: f32 = 26.0;

/// 卡片内容最大宽度（超宽窗口下避免控件横向拉得过散）。
const CONTENT_MAX_WIDTH: f32 = 480.0;

/// 绘制全部设置项（在 `redraw` 闭包内调用，编辑局部 `draft`/`dir_input`）。
///
/// 布局为 macOS 系统设置风格：左侧导航栏 + 右侧内容区（大标题 + 圆角卡片）。
/// 卡片内为「左标签 + 右控件」的行式排布，行间细分隔线。
fn draw_settings_ui(
    ui: &mut egui::Ui,
    draft: &mut Config,
    dir_input: &mut String,
    active_section: &mut Section,
    recording: bool,
    status: Option<&(bool, String)>,
    changed: &mut bool,
    start_recording: &mut bool,
    mdl: &mut ModelDownloadUi,
    font_pickers: &mut std::collections::HashMap<String, super::font_list::FontPickerState>,
    params_ed: &mut ParamsEditUi,
) {
    let pal = palette(matches!(draft.ui.theme, Theme::Dark));

    // 根 Ui 无自带背景填充（渲染栈清屏色是黑色），按主题铺满页面底色——
    // 否则浅色主题下窗口依旧透黑（2026-08-20 用户实机反馈）
    let full = ui.max_rect();
    ui.painter().rect_filled(full, 0.0, pal.page_bg);

    // 左侧栏：底色 + 右侧分隔线
    let sidebar_rect = egui::Rect::from_min_size(full.min, egui::vec2(SIDEBAR_WIDTH, full.height()));
    ui.painter().rect_filled(sidebar_rect, 0.0, pal.sidebar_bg);
    ui.painter().vline(
        sidebar_rect.right(),
        sidebar_rect.top()..=sidebar_rect.bottom(),
        egui::Stroke::new(1.0, pal.card_stroke),
    );
    let mut side_ui = ui.new_child(
        egui::UiBuilder::new().max_rect(sidebar_rect.shrink2(egui::vec2(14.0, 16.0))),
    );
    draw_sidebar(&mut side_ui, &pal, active_section);

    // 右侧内容区：独立滚动
    let content_rect = egui::Rect::from_min_max(
        egui::pos2(full.min.x + SIDEBAR_WIDTH, full.min.y),
        full.max,
    );
    let mut content_ui = ui.new_child(egui::UiBuilder::new().max_rect(content_rect));
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(&mut content_ui, |ui| {
            ui.add_space(24.0);
            ui.horizontal(|ui| {
                ui.add_space(CONTENT_MARGIN);
                ui.label(
                    egui::RichText::new(active_section.title())
                        .size(22.0)
                        .strong(),
                );
            });
            ui.add_space(16.0);

            match *active_section {
                Section::General => {
                    draw_general(ui, &pal, draft, recording, start_recording, changed)
                }
                Section::Save => draw_save(ui, &pal, draft, dir_input, changed),
                Section::Capture => draw_capture(ui, &pal, draft, changed),
                Section::Appearance => draw_appearance(ui, &pal, draft, changed, font_pickers),
                Section::Ai => draw_ai(ui, &pal, draft, changed, mdl, params_ed),
            }

            // 状态提示（保存成功/失败等）
            if let Some((ok, msg)) = status {
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.add_space(CONTENT_MARGIN);
                    let color = if *ok {
                        egui::Color32::from_rgb(52, 199, 89)
                    } else {
                        egui::Color32::from_rgb(255, 69, 58)
                    };
                    ui.label(egui::RichText::new(msg).size(12.0).color(color));
                });
            }
            ui.add_space(24.0);
        });
}

/// 左侧导航栏：应用名 + 分区列表（圆角胶囊高亮选中项）。
fn draw_sidebar(ui: &mut egui::Ui, pal: &Palette, active: &mut Section) {
    ui.label(egui::RichText::new("PrismaSnap").size(16.0).strong());
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new("截图 · 标注 · AI")
            .size(11.0)
            .color(pal.secondary),
    );
    ui.add_space(16.0);
    for section in Section::ALL {
        nav_item(ui, pal, section, active);
    }
}

/// 侧栏单个导航项。
fn nav_item(ui: &mut egui::Ui, pal: &Palette, section: Section, active: &mut Section) {
    let selected = *active == section;
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 30.0),
        egui::Sense::click(),
    );
    if selected {
        ui.painter().rect_filled(rect, 7.0, pal.nav_selected);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 7.0, pal.nav_hover);
    }
    let color = if selected {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().text_color()
    };
    ui.painter().text(
        egui::pos2(rect.left() + 10.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        section.label(),
        egui::FontId::proportional(if selected { 13.5 } else { 13.0 }),
        color,
    );
    if response.clicked() {
        *active = section;
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand);
    ui.add_space(3.0);
}

/// 「通用」分区：截图热键。
fn draw_general(
    ui: &mut egui::Ui,
    pal: &Palette,
    draft: &mut Config,
    recording: bool,
    start_recording: &mut bool,
    changed: &mut bool,
) {
    card(ui, pal, |ui| {
        setting_row(ui, "截图热键", |ui| {
            if recording {
                ui.label(
                    egui::RichText::new("请按下组合键（Esc 取消）")
                        .size(12.5)
                        .color(pal.accent),
                );
            } else {
                // 控件从右往左排：最右是当前热键键帽，其左是重新录制按钮
                if primary_button(ui, pal, "重新录制").clicked() {
                    *start_recording = true;
                }
                ui.add_space(8.0);
                keycap(ui, pal, &display_hotkey(&draft.hotkey));
            }
        });
        row_separator(ui, pal);
        setting_row(ui, "日志级别（需重启）", |ui| {
            *changed |= segmented(
                ui,
                pal,
                &[
                    ("error", LogLevel::Error),
                    ("warn", LogLevel::Warn),
                    ("info", LogLevel::Info),
                    ("debug", LogLevel::Debug),
                    ("trace", LogLevel::Trace),
                ],
                56.0,
                &mut draft.logging.level,
            );
        });
    });
}

/// 「保存」分区：保存模式 / 目录 / 格式 / 质量。
fn draw_save(
    ui: &mut egui::Ui,
    pal: &Palette,
    draft: &mut Config,
    dir_input: &mut String,
    changed: &mut bool,
) {
    card(ui, pal, |ui| {
        setting_row(ui, "保存模式", |ui| {
            *changed |= segmented(
                ui,
                pal,
                &[
                    ("静默保存", SaveMode::Silent),
                    ("每次询问", SaveMode::AlwaysAsk),
                ],
                56.0,
                &mut draft.save.mode,
            );
        });
        row_separator(ui, pal);

        setting_row(ui, "保存目录", |ui| {
            // 自适应宽：按路径文本实测 + 边距；下限使**外框**与上下 2 档 segmented
            // 可视宽严格相等（短路径时三行等长——2026-09-12 用户实机反馈；
            // framed_singleline 外宽 = 内容宽 + 左右 margin 6×2 + 描边 2 = +14），
            // 长路径按文本加宽、上限 300（再长截右侧，不和左侧标签重叠）。
            let text_w = ui
                .painter()
                .layout_no_wrap(
                    dir_input.clone(),
                    egui::FontId::proportional(12.5),
                    egui::Color32::TRANSPARENT,
                )
                .size()
                .x;
            // text_w + 20：文本两侧各留约 10px（6 margin + 1 描边 + 3 余量）——
            // 此前 +28 余量会让普通路径比下限多 1px、外框比上下框宽（第二轮实机反馈）
            let w = (text_w + 20.0).clamp(segmented_width(2, 56.0) - 14.0, 300.0);
            *changed |= framed_singleline(
                ui,
                pal,
                dir_input,
                w,
                Some("<程序目录>/screenshots"),
                false,
            )
            .changed();
        });
        row_separator(ui, pal);

        setting_row(ui, "图片格式", |ui| {
            *changed |= segmented(
                ui,
                pal,
                &[("PNG", SaveFormat::Png), ("JPEG", SaveFormat::Jpeg)],
                56.0,
                &mut draft.save.format,
            );
        });

        if draft.save.format == SaveFormat::Jpeg {
            row_separator(ui, pal);
            setting_row(ui, "JPEG 质量", |ui| {
                let value_text = format!("{}", draft.save.jpeg_quality);
                *changed |= slider_total_width(
                    ui,
                    182.0 + 48.0,
                    &value_text,
                    egui::Slider::new(&mut draft.save.jpeg_quality, 1..=100).show_value(true),
                )
                .changed();
            });
        }
    });
}

/// 「捕获」分区：光标开关。
fn draw_capture(ui: &mut egui::Ui, pal: &Palette, draft: &mut Config, changed: &mut bool) {
    card(ui, pal, |ui| {
        setting_row(ui, "截图包含光标", |ui| {
            *changed |= toggle(ui, pal, &mut draft.capture.cursor_visible);
        });
    });
}

/// 「外观」分区：主题 + 界面字体 + 标注/翻译字体（2026-09-09 用户需求）。
fn draw_appearance(
    ui: &mut egui::Ui,
    pal: &Palette,
    draft: &mut Config,
    changed: &mut bool,
    font_pickers: &mut std::collections::HashMap<String, super::font_list::FontPickerState>,
) {
    card(ui, pal, |ui| {
        setting_row(ui, "主题", |ui| {
            *changed |= segmented(
                ui,
                pal,
                &[("浅色", Theme::Light), ("深色", Theme::Dark)],
                56.0,
                &mut draft.ui.theme,
            );
        });
        let dark = matches!(draft.ui.theme, Theme::Dark);
        let style = super::font_list::PickerStyle {
            fill: pal.control_bg,
            stroke: egui::Stroke::new(1.0, pal.separator),
            text: if dark { egui::Color32::from_rgb(240, 240, 245) } else { egui::Color32::from_rgb(26, 26, 28) },
            popup_fill: pal.card_bg,
            popup_stroke: egui::Stroke::new(1.0, pal.card_stroke),
        };
        // 界面字体：设置窗口与覆盖层 UI（改后即时重装当前 ctx；覆盖层下次截图生效）
        row_separator(ui, pal);
        setting_row(ui, "界面字体", |ui| {
            if font_pick_row(ui, "interface", style, &mut draft.ui.interface_font, font_pickers) {
                crate::utils::fontsel::set_interface_font((!draft.ui.interface_font.is_empty())
                    .then(|| draft.ui.interface_font.clone()));
                super::gui::install_cjk_font(ui.ctx());
                *changed = true;
            }
        });
        // 标注/翻译字体：文字标注 + 译文覆盖（导出与预览同字体，所见即所得）
        row_separator(ui, pal);
        setting_row(ui, "标注与翻译字体", |ui| {
            if font_pick_row(ui, "annotation", style, &mut draft.ui.annotation_font, font_pickers) {
                crate::utils::fontsel::set_annotation_font((!draft.ui.annotation_font.is_empty())
                    .then(|| draft.ui.annotation_font.clone()));
                super::gui::install_cjk_font(ui.ctx());
                *changed = true;
            }
        });
    });
}

/// 单行字体选择（共享弹层组件；返回是否发生选择提交）。
fn font_pick_row(
    ui: &mut egui::Ui,
    key: &str,
    style: super::font_list::PickerStyle,
    value: &mut String,
    font_pickers: &mut std::collections::HashMap<String, super::font_list::FontPickerState>,
) -> bool {
    let fonts = crate::utils::fontsel::list_fonts();
    if fonts.is_empty() {
        ui.add(egui::Label::new(egui::RichText::new("系统默认").weak()));
        return false;
    }
    let state = font_pickers.entry(key.to_string()).or_default();
    let display = if value.is_empty() {
        "系统默认（微软雅黑）".to_string()
    } else {
        super::font_list::display_name_for(value)
    };
    if let super::font_list::FontPickOutcome::Committed(picked) = super::font_list::font_picker_widget(
        ui,
        key,
        state,
        style,
        &display,
        // 116 = 主题分段槽总宽（56×2+槽边距 4），同行视觉对齐
        116.0,
        true,
    ) {
        // 提交：None=系统默认，Some(path)=选字体
        *value = picked.unwrap_or_default();
        return true;
    }
    false
}

/// 「AI 接口」分区：基础 LLM（文本翻译后端，布局保持不动）+ 多模态 LLM
/// （三选一）+ 翻译模式 + OCR 引擎（见 AGENTS.md 3.8 节）。
fn draw_ai(
    ui: &mut egui::Ui,
    pal: &Palette,
    draft: &mut Config,
    changed: &mut bool,
    mdl: &mut ModelDownloadUi,
    params_ed: &mut ParamsEditUi,
) {
    // 卡片一：基础 LLM（即文本翻译后端；行布局保持不动，只改绑定到新配置）。
    card(ui, pal, |ui| {
        setting_row(ui, "API 地址", |ui| {
            *changed |= framed_singleline(
                ui,
                pal,
                &mut draft.translate.text_llm.api_url,
                228.0,
                Some("http://127.0.0.1:8080/v1/chat/completions"),
                false,
            )
            .changed();
        });
        row_separator(ui, pal);
        setting_row(ui, "API Key", |ui| {
            *changed |= framed_singleline(
                ui,
                pal,
                &mut draft.translate.text_llm.api_key,
                228.0,
                None,
                true,
            )
            .changed();
        });
        row_separator(ui, pal);
        setting_row(ui, "模型", |ui| {
            *changed |= framed_singleline(
                ui,
                pal,
                &mut draft.translate.text_llm.model,
                228.0,
                None,
                false,
            )
            .changed();
        });
        row_separator(ui, pal);
        setting_row(ui, "翻译目标", |ui| {
            *changed |= framed_singleline(
                ui,
                pal,
                &mut draft.translate.target_lang,
                228.0,
                Some("简体中文"),
                false,
            )
            .changed();
        });
    });
    ui.add_space(12.0);

    // 卡片二：多模态 LLM（三选一，默认与基础相同；自定义时展开独立配置）。
    card(ui, pal, |ui| {
        setting_row(ui, "多模态", |ui| {
            *changed |= segmented(
                ui,
                pal,
                &[
                    ("与基础相同", MultimodalMode::SameAsText),
                    ("不配置", MultimodalMode::Disabled),
                    ("自定义", MultimodalMode::Custom),
                ],
                80.0,
                &mut draft.translate.multimodal_llm.mode,
            );
        });
        if draft.translate.multimodal_llm.mode == MultimodalMode::Custom {
            row_separator(ui, pal);
            setting_row(ui, "API 地址", |ui| {
                *changed |= framed_singleline(
                    ui,
                    pal,
                    &mut draft.translate.multimodal_llm.api_url,
                    228.0,
                    Some("多模态模型地址"),
                    false,
                )
                .changed();
            });
            row_separator(ui, pal);
            setting_row(ui, "API Key", |ui| {
                *changed |= framed_singleline(
                    ui,
                    pal,
                    &mut draft.translate.multimodal_llm.api_key,
                    228.0,
                    None,
                    true,
                )
                .changed();
            });
            row_separator(ui, pal);
            setting_row(ui, "模型", |ui| {
                *changed |= framed_singleline(
                    ui,
                    pal,
                    &mut draft.translate.multimodal_llm.model,
                    228.0,
                    Some("如 qwen-vl-max"),
                    false,
                )
                .changed();
            });
        }
    });
    ui.add_space(12.0);

    // 卡片三：翻译模式（三选一，默认自动；Auto 才显示阈值滑块；
    // 提示图标紧跟标签右侧，点击弹说明窗，悬停看一句话简述）。
    card(ui, pal, |ui| {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("翻译模式").size(13.0));
            let tip = hint_icon_button(ui, "翻译模式说明（点击看详情）：自动按置信度分流；OCR 文本快而便宜；裁剪多模态艺术字更准但贵。");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                *changed |= segmented(
                    ui,
                    pal,
                    &[
                        ("自动", TranslateMode::Auto),
                        ("OCR 文本", TranslateMode::OcrText),
                        ("裁剪多模态", TranslateMode::CropMultimodal),
                    ],
                    80.0,
                    &mut draft.translate.mode,
                );
            });
            egui::Popup::menu(&tip).show(|ui| {
                // 与设置菜单同风格：卡片底 + 描边 + 圆角
                egui::Frame::new()
                    .fill(pal.card_bg)
                    .stroke(egui::Stroke::new(1.0, pal.card_stroke))
                    .corner_radius(10.0)
                    .inner_margin(egui::Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_width(280.0);
                    ui.label(egui::RichText::new("翻译模式说明").size(12.5).strong());
                    ui.separator();
                    for (i, (name, desc)) in [
                        ("自动", "按置信度分流，有把握走纯文本，没把握裁剪给多模态（默认）"),
                        ("OCR 文本", "先识别再整体翻译，快、便宜，适合界面文档等标准字体"),
                        ("裁剪多模态", "逐框裁剪给多模态识别+翻译，艺术字更准，但贵而慢"),
                    ]
                    .iter()
                    .enumerate()
                    {
                        if i > 0 {
                            ui.separator();
                        }
                        ui.label(egui::RichText::new(*name).size(12.5).strong());
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(*desc).size(12.0).color(pal.secondary),
                            )
                            .wrap(),
                        );
                    }
                    });
            });
        });
        if draft.translate.mode == TranslateMode::Auto {
            row_separator(ui, pal);
            setting_row(ui, "置信度阈值", |ui| {
                // 目标："数字框 + 轨道"总宽 = 上方 3 档分段槽（2026-09-12 用户要求；
                // 轨道长度靠 spacing.slider_width，数字框宽见 drag_value_width）
                let value_text = format!("{:.3}", draft.translate.confidence_threshold);
                *changed |= slider_total_width(
                    ui,
                    segmented_width(3, 80.0),
                    &value_text,
                    egui::Slider::new(&mut draft.translate.confidence_threshold, 0.5..=0.95)
                        .show_value(true),
                )
                .changed();
            });
        }
        // 多模态未配置却选了裁剪模式：行内警告（功能侧同样会降级，见 3.8 节）。
        if draft.translate.mode == TranslateMode::CropMultimodal
            && draft.translate.multimodal_llm.mode == MultimodalMode::Disabled
        {
            row_separator(ui, pal);
            setting_row(ui, "提示", |ui| {
                ui.label(
                    egui::RichText::new("多模态未配置，该模式不可用")
                        .size(12.5)
                        .color(egui::Color32::from_rgb(255, 69, 58)),
                );
            });
        }
    });
    ui.add_space(12.0);

    // 卡片四：OCR 引擎（自动优先插件 + 状态行）。
    card(ui, pal, |ui| {
        setting_row(ui, "OCR 引擎", |ui| {
            *changed |= segmented(
                ui,
                pal,
                &[
                    ("自动", OcrEngineKind::Auto),
                    ("系统", OcrEngineKind::System),
                    ("插件", OcrEngineKind::Rapidocr),
                ],
                56.0,
                &mut draft.ocr.engine,
            );
        });
        row_separator(ui, pal);
        let engine = create_engine(&draft.ocr.engine);
        let (status, ok) = if engine.is_available() {
            (format!("当前引擎：{}（可用）", engine.name()), true)
        } else {
            (format!("当前引擎：{}（不可用）", engine.name()), false)
        };
        setting_row(ui, "OCR 状态", |ui| {
            let color = if ok {
                egui::Color32::from_rgb(52, 199, 89)
            } else {
                egui::Color32::from_rgb(255, 69, 58)
            };
            ui.label(egui::RichText::new(status).size(12.5).color(color));
        });
    });
    ui.add_space(12.0);

    // 卡片五：OCR 插件模型（四件套下载：官方源自动下载 + 手动下载帮助）。
    draw_model_card(ui, pal, mdl);
    ui.add_space(12.0);

    // 卡片六：大模型参数（温度/上限/思考模式开关 + 自定义思考 JSON + 重置）。
    draw_params_card(ui, pal, draft, changed, params_ed);
    ui.add_space(12.0);

    // 卡片七：提示词模板（纯文本/多模态单图/多模态批量；留空用内置默认）。
    card(ui, pal, |ui| {
        ui.add_space(4.0);
        ui.label(egui::RichText::new("提示词模板").size(13.0));
        // "占位符"前加提示图标，完整说明进悬停提示（行内只留标签，干净）
        ui.horizontal(|ui| {
            hint_icon(
                ui,
                "占位符：{target} 目标语言，{items} 输入条目（纯文本），{count} 图片数（批量）。留空即用内置默认。",
            );
            ui.label(
                egui::RichText::new("占位符")
                    .size(12.0)
                    .color(pal.secondary),
            );
        });
        ui.add_space(4.0);
        for (label, field) in [
            ("纯文本", &mut draft.translate.prompts.text),
            ("多模态单图", &mut draft.translate.prompts.multimodal_single),
            ("多模态批量", &mut draft.translate.prompts.multimodal_batch),
        ] {
            ui.label(egui::RichText::new(label).size(12.5).strong());
            *changed |= framed_multiline(ui, pal, field, 84.0, label).changed();
            ui.add_space(4.0);
        }
        if primary_button(ui, pal, "恢复默认提示词").clicked() {
            draft.translate.prompts = TranslatePrompts::default();
            *changed = true;
        }
        ui.add_space(4.0);
    });
}

/// 卡片五：OCR 插件模型（四件套：每行独立下载/暂停/继续/取消 + 顶部取消全部）。
///
/// 文件状态每次绘制现查（`download::file_states`），下好放进 `plugins/ocr/`
/// 后本页自动变绿；引擎行（卡片四）同样按帧计算，无需重启、无需"重新检测"
///（唯一例外：之前放错 DLL 并触发过识别/翻译的，需重启——运行时只初始化一次）。
fn draw_model_card(ui: &mut egui::Ui, pal: &Palette, mdl: &mut ModelDownloadUi) {
    card(ui, pal, |ui| {
        // 标题行：左"OCR插件下载"+提示图标（点击开手动下载帮助，悬停看简述），
        // 右仅保留取消（有任务才显示）；"官方自动下载"说明文字已删（默认行为）。
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("OCR插件下载").size(13.0));
            if hint_icon_button(
                ui,
                "插件模型从官方源自动下载；点击打开手动下载帮助（含项目地址/直链/改名对照）。",
            )
            .clicked()
            {
                mdl.show_help = true;
                mdl.copied = None;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if mdl.any_job() && ui.small_button("取消").clicked() {
                    mdl.cancel_all();
                }
            });
        });
        ui.add_space(4.0);
        row_separator(ui, pal);
        // 四文件行：本地名 + 说明 | 右侧：已就绪 / 下载 / 暂停+取消 / 继续+取消
        let dir = ocr_plugin_dir().unwrap_or_else(|_| PathBuf::from("plugins/ocr"));
        let files = download::model_files();
        let states = download::file_states(&dir);
        for (i, f) in files.iter().enumerate() {
            let row = format!("{}（{}）", f.local, f.desc);
            setting_row(ui, &row, |ui| {
                ui.horizontal(|ui| {
                    let is_ready = states[i] == FileState::Ready;
                    match mdl.jobs[i].as_ref() {
                        // 无任务：就绪显示绿字，否则"下载"按钮
                        None => {
                            if is_ready {
                                ui.label(
                                    egui::RichText::new(FileState::Ready.label())
                                        .size(12.5)
                                        .color(egui::Color32::from_rgb(52, 199, 89)),
                                );
                            } else if ui.small_button("下载").clicked() {
                                mdl.start(i);
                            }
                        }
                        Some(job) if job.live => {
                            // 在跑：进度 + 暂停 + 取消（点下载后按钮即换成这俩）
                            ui.label(
                                egui::RichText::new(progress_text(job.progress))
                                    .size(12.5)
                                    .color(egui::Color32::from_rgb(0, 122, 255)),
                            );
                            if ui.small_button("暂停").clicked() {
                                mdl.pause(i);
                            }
                            if ui.small_button("取消").clicked() {
                                mdl.cancel(i);
                            }
                        }
                        Some(job) => {
                            // 已暂停：继续 + 取消（取消删 .part，行回初始"下载"）
                            ui.label(
                                egui::RichText::new(format!(
                                    "已暂停 {}",
                                    job.progress.map(|(d, _)| human_bytes(d)).unwrap_or_default()
                                ))
                                .size(12.5)
                                .color(pal.secondary),
                            );
                            if ui.small_button("继续").clicked() {
                                mdl.resume(i);
                            }
                            if ui.small_button("取消").clicked() {
                                mdl.cancel(i);
                            }
                        }
                    }
                });
            });
            if i + 1 < files.len() {
                row_separator(ui, pal);
            }
        }
        // 全局结果消息（失败含手动下载指引；成功只在四齐时报一次）
        if let Some((ok, msg)) = mdl.result.clone() {
            row_separator(ui, pal);
            setting_row(ui, "结果", |ui| {
                let color = if ok {
                    egui::Color32::from_rgb(52, 199, 89)
                } else {
                    egui::Color32::from_rgb(255, 69, 58)
                };
                ui.label(egui::RichText::new(msg).size(12.0).color(color));
            });
        }
        ui.add_space(4.0);
    });
    // 帮助窗口（可拖动 egui::Window，标题栏拖移；主题跟随设置页）
    if mdl.show_help {
        draw_model_help(ui.ctx(), pal, mdl);
    }
}

/// 下载进度文字（有总长显示百分比，否则显示已下字节）。
fn progress_text(progress: Option<(u64, Option<u64>)>) -> String {
    match progress {
        Some((done, Some(total))) if total > 0 => format!(
            "下载中 {}%",
            (done as f64 / total as f64 * 100.0).floor() as u64
        ),
        Some((done, _)) => format!("下载中 {}", human_bytes(done)),
        None => String::from("准备中…"),
    }
}

/// 手动下载帮助：项目地址 + 四文件直链（可复制）+ 改名对照 + 校验说明。
fn draw_model_help(ctx: &egui::Context, pal: &Palette, mdl: &mut ModelDownloadUi) {
    let mut open = mdl.show_help;
    // 与设置菜单同风格：卡片底 + 描边 + 圆角（主题跟随设置页）
    egui::Window::new("手动下载帮助")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(520.0)
        .frame(
            egui::Frame::new()
                .fill(pal.card_bg)
                .stroke(egui::Stroke::new(1.0, pal.card_stroke))
                .corner_radius(10.0)
                .inner_margin(egui::Margin::same(14)),
        )
        .show(ctx, |ui| {
            ui.label(egui::RichText::new("项目地址").size(13.0).strong());
            for (name, url) in [
                ("RapidOCR（模型来源）", RAPIDOCR_REPO_URL),
                ("模型托管页（可网页下载）", MODELSCOPE_PAGE_URL),
                ("ONNX Runtime 发布页（DLL，选 v1.28.1 win-x64）", ORT_RELEASES_URL),
            ] {
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new(name).size(12.5));
                    if ui.small_button("复制链接").clicked()
                        && crate::utils::clipboard::copy_text(url).is_ok() {
                            mdl.copied = Some(url.to_string());
                        }
                });
                ui.label(egui::RichText::new(url).size(11.5).color(egui::Color32::GRAY));
                ui.add_space(2.0);
            }
            ui.separator();
            ui.label(egui::RichText::new("四文件直链（下好后改名放入 plugins/ocr/）").size(13.0).strong());
            for f in download::model_files() {
                ui.add_space(4.0);
                let target = if f.url.is_some() {
                    format!("{} ← {}", f.local, f.origin)
                } else {
                    format!("{} ← {}（zip 包，解出 DLL）", f.local, f.origin)
                };
                ui.label(egui::RichText::new(target).size(12.5).strong());
                let url = f.url.or(f.zip_url).unwrap_or("");
                if ui.small_button("复制直链").clicked()
                    && crate::utils::clipboard::copy_text(url).is_ok()
                {
                    mdl.copied = Some(url.to_string());
                }
                if let Some(h) = f.sha256 {
                    ui.label(
                        egui::RichText::new(format!("SHA256: {h}"))
                            .size(11.0)
                            .color(egui::Color32::GRAY),
                    );
                }
            }
            ui.separator();
            ui.label(
                egui::RichText::new(
                    "步骤：① 按上表下载 4 个文件并改名；② 放入程序目录 plugins/ocr/；\n\
                     ③ 回到本页，文件行自动变绿即就绪（字典须与识别模型配套，错配会被拦截并提示）。\n\
                     注意：如之前放错过 DLL 并点过识别/翻译，需重启程序（运行时只初始化一次）。",
                )
                .size(12.0),
            );
            if let Some(c) = mdl.copied.clone() {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(format!("已复制：{c}"))
                        .size(12.0)
                        .color(egui::Color32::from_rgb(52, 199, 89)),
                );
            }
        });
    mdl.show_help = open;
}

/// 字节数转人类可读（下载进度用）。
fn human_bytes(n: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if n as f64 >= MB {
        format!("{:.1}MB", n as f64 / MB)
    } else {
        format!("{}KB", n / 1024)
    }
}

/// 圆角卡片容器（内容区固定左右留白 + 最大宽度）。
fn card(ui: &mut egui::Ui, pal: &Palette, rows: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(pal.card_bg)
        .stroke(egui::Stroke::new(1.0, pal.card_stroke))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(14, 4))
        .outer_margin(egui::Margin::symmetric(CONTENT_MARGIN as i8, 0))
        .show(ui, |ui| {
            let width = ui.available_width().min(CONTENT_MAX_WIDTH);
            ui.set_width(width);
            rows(ui);
        });
}

/// 卡片内一行：左侧标签 + 右侧控件（垂直居中，右对齐）。
fn setting_row(ui: &mut egui::Ui, label: &str, add_control: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).size(13.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            add_control(ui);
        });
    });
    ui.add_space(4.0);
}

/// 卡片内行间分隔线（整行宽）。
fn row_separator(ui: &mut egui::Ui, pal: &Palette) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 0.0, pal.separator);
}

/// 右控件区可视高度（含 1px 描边）：实际文本行高 + 上下 4 边距 + Frame 描边 2。
///
/// 2026-09-12 用户多轮反馈 segmented / 输入框 / toggle / 滑条与键帽不等高——
/// 此前各控件写死魔数（21/24/25），实际行高随界面字体浮动。改为统一按当前
/// 字体实测动态计算，数学上保证右控件区可视高严格相等。
/// 注：egui `Frame` 的 `outer_rect` 会把 stroke 宽度计入总高（上下各 +1），
/// 故 Frame 类控件的内高要再减 2（见各调用处 `- 6`）；
/// headless 单测 `control_heights_are_uniform` 锁定。
fn control_h(ui: &egui::Ui) -> f32 {
    text_row_h(ui) + 10.0
}

/// 12.5pt 文本的**实际渲染行高**（galley 高度，与 Label/TextEdit 同源）。
///
/// 不用 `fonts.row_height`：实测（egui 0.36 默认字体）它为 14.38，而 Label 的
/// galley 高 14.00——实机字体下偏差方向/大小不定，会把键帽文字撑高、与分段槽
/// 差 1~2px（第二轮实机反馈的残留根因），改用与文字渲染完全一致的 galley 高。
fn text_row_h(ui: &egui::Ui) -> f32 {
    ui.painter()
        .layout_no_wrap(
            String::from("Ag"),
            egui::FontId::proportional(12.5),
            egui::Color32::WHITE,
        )
        .size()
        .y
}

/// 分段选择器可视总宽（`count` 档 × 按钮宽 + 槽内边距 2×2 + 描边 1×2），
/// 供同行滑条/输入框对齐（egui `Frame::outer_rect` 会把 stroke 计入尺寸）。
fn segmented_width(count: usize, button_width: f32) -> f32 {
    count as f32 * button_width + 6.0
}

/// 估算 Slider 内数字框（`DragValue`）的宽度。
///
/// egui `DragValue`：内容 = 值文本，左右边距 = `button_padding`，下限 =
/// `interact_size.x`（默认 40）。用于把"数字框 + 轨道"总宽反推为分段槽宽
/// （2026-09-12 用户要求"滑条加数字总共和上面一样长"）。
fn drag_value_width(ui: &egui::Ui, text: &str) -> f32 {
    let text_w = ui
        .painter()
        .layout_no_wrap(
            text.to_owned(),
            egui::FontId::proportional(12.5),
            egui::Color32::PLACEHOLDER,
        )
        .size()
        .x;
    (text_w + ui.spacing().button_padding.x * 2.0).max(ui.spacing().interact_size.x)
}

/// 把某个滑条布置成"数字框 + 轨道"总宽 = `total`（右对齐时与上方分段槽等长），
/// 并统一行高为 `control_h`。返回滑条响应。
fn slider_total_width(
    ui: &mut egui::Ui,
    total: f32,
    value_text: &str,
    slider: egui::Slider<'_>,
) -> egui::Response {
    // 数字框（DragValue 内部是 Button）圆角与全设置页统一为 6
    // （2026-09-12 用户实机反馈：默认 2 太方，与上下控件不一致）
    let radius = egui::CornerRadius::same(6);
    let widgets = &mut ui.style_mut().visuals.widgets;
    widgets.inactive.corner_radius = radius;
    widgets.hovered.corner_radius = radius;
    widgets.active.corner_radius = radius;
    widgets.noninteractive.corner_radius = radius;
    let value_w = drag_value_width(ui, value_text);
    ui.spacing_mut().slider_width =
        (total - value_w - ui.spacing().item_spacing.x).max(120.0);
    ui.set_min_height(control_h(ui));
    ui.add(slider)
}

/// 分段选择器（macOS 胶囊组），返回是否变更。
///
/// `button_width` 为整排统一固定宽度——各选项按内容自适应时，选中加粗与
/// 长标签（如"与基础相同"）会让整排宽度随状态抖动（2026-09-03 用户实机反馈），
/// 固定宽度后各态尺寸恒定。
fn segmented<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    pal: &Palette,
    options: &[(&str, T)],
    button_width: f32,
    value: &mut T,
) -> bool {
    let mut changed = false;
    let h = control_h(ui);
    egui::Frame::new()
        .fill(pal.control_bg)
        .stroke(egui::Stroke::new(1.0, pal.card_stroke))
        .corner_radius(7.0)
        .inner_margin(egui::Margin::same(2))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for (label, v) in options {
                let selected = *value == *v;
                let text = if selected {
                    egui::RichText::new(*label).size(12.5).strong()
                } else {
                    egui::RichText::new(*label).size(12.5).color(pal.secondary)
                };
                let btn = egui::Button::new(text)
                    .fill(if selected {
                        pal.card_bg
                    } else {
                        egui::Color32::TRANSPARENT
                    })
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(6.0);
                // 槽内边距 2×2 + Frame 描边 2：按钮高 = control_h - 6，
                // 字号/行高变化时自动对齐（见 control_h 注释）
                if ui.add_sized(egui::vec2(button_width, h - 6.0), btn).clicked() {
                    *value = *v;
                    changed = true;
                }
            }
        });
    changed
}

/// 键帽徽章（展示当前热键，弱控件底色 + 细描边）。
///
/// 键帽是右控件区的**高度基准**：内容高锁定为 [`control_h`] - 6（Frame 上下
/// 边距 4 + 描边 2），文字用 painter 居中绘制、**不参与布局**——任何字体行高
/// /DPI 下键帽总高都恒等于动态计算的 `control_h`；日志级别等分段槽复用同一
/// 基准（2026-09-12 用户要求"日志级别复用截图热键的高度"，是动态基准而非
/// 硬编码像素，换设备/分辨率不会跑偏）。
fn keycap(ui: &mut egui::Ui, pal: &Palette, text: &str) {
    let h = control_h(ui);
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        egui::FontId::proportional(12.5),
        ui.visuals().strong_text_color(),
    );
    let text_w = galley.size().x;
    egui::Frame::new()
        .fill(pal.control_bg)
        .stroke(egui::Stroke::new(1.0, pal.card_stroke))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(10, 2))
        .show(ui, |ui| {
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(text_w, h - 6.0), egui::Sense::hover());
            ui.painter().galley(
                rect.center() - galley.size() * 0.5,
                galley,
                egui::Color32::PLACEHOLDER,
            );
        });
}

/// 强调色主按钮（白字 + accent 底，高度与右控件区统一）。
fn primary_button(ui: &mut egui::Ui, pal: &Palette, text: &str) -> egui::Response {
    let h = control_h(ui);
    ui.add(
        egui::Button::new(egui::RichText::new(text).size(12.5).color(egui::Color32::WHITE))
            .fill(pal.accent)
            .stroke(egui::Stroke::NONE)
            .corner_radius(6.0)
            .min_size(egui::vec2(56.0, h)),
    )
}

/// iOS 风格开关，返回是否变更（轨道 40×control_h，与右区其他控件同高）。
fn toggle(ui: &mut egui::Ui, pal: &Palette, on: &mut bool) -> bool {
    let h = control_h(ui);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(40.0, h), egui::Sense::click());
    let mut changed = false;
    if response.clicked() {
        *on = !*on;
        changed = true;
    }
    let anim = ui.ctx().animate_bool_with_time(response.id, *on, 0.15);
    let track = pal.control_bg.lerp_to_gamma(pal.accent, anim);
    ui.painter().rect_filled(rect, h / 2.0, track);
    // 轨道细描边：与输入框/键帽/分段槽视觉高度对齐（2026-09-12 用户实机反馈）
    ui.painter().rect_stroke(
        rect,
        h / 2.0,
        egui::Stroke::new(1.0, pal.card_stroke),
        egui::StrokeKind::Inside,
    );
    let knob_r = h / 2.0 - 3.5;
    let knob_x = egui::emath::lerp(rect.left() + h / 2.0..=rect.right() - h / 2.0, anim);
    ui.painter().circle_filled(egui::pos2(knob_x, rect.center().y), knob_r, egui::Color32::WHITE);
    response.on_hover_cursor(egui::CursorIcon::PointingHand);
    changed
}

/// 行内提示图标资源（200×200 RGBA；`include_bytes!` 编进 exe，便携无外部依赖）。
const ICON_HINT: &[u8] = include_bytes!("../../assets/icons/提示.png");

/// 提示图标边长（逻辑点，比 13pt 行高略大一点，不撑行高）。
const HINT_ICON_SIZE: f32 = 14.0;

/// 取提示图标纹理（ctx 数据区缓存，多帧复用不重复解码；失败返回 `None`，
/// 调用方直接跳过图标，功能不受影响）。
fn hint_icon_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let id = egui::Id::new("settings_hint_icon");
    if let Some(handle) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return Some(handle);
    }
    let rgba = image::load_from_memory(ICON_HINT).ok()?.to_rgba8();
    let color = egui::ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        rgba.as_raw(),
    );
    let handle = ctx.load_texture("settings_hint_icon", color, egui::TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(id, handle.clone()));
    Some(handle)
}

/// 行内小提示图标（悬停出 tooltip；解码失败时静默跳过）。
fn hint_icon(ui: &mut egui::Ui, tooltip: &str) {
    if let Some(handle) = hint_icon_texture(ui.ctx()) {
        ui.add(egui::Image::new(&handle).fit_to_exact_size(egui::vec2(HINT_ICON_SIZE, HINT_ICON_SIZE)))
            .on_hover_text(tooltip);
    }
}

/// 可点击的提示图标（行为同"?"按钮：点击供调用方弹说明窗，悬停看简述；
/// 解码失败时回退文字 "?"，功能不断）。
fn hint_icon_button(ui: &mut egui::Ui, tooltip: &str) -> egui::Response {
    if let Some(handle) = hint_icon_texture(ui.ctx()) {
        ui.add(
            egui::Image::new(&handle)
                .fit_to_exact_size(egui::vec2(HINT_ICON_SIZE, HINT_ICON_SIZE))
                .sense(egui::Sense::click()),
        )
        .on_hover_text(tooltip)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        ui.small_button("?")
    }
}

/// 启用自定义参数开关的悬停提示（只保留开/关作用说明）。
const THINK_TIP: &str = "打开：请求时合并下方自定义参数（含思考四键）；\n关闭：剔掉思考四键后发送。";

/// 带边框的单行输入框（与 `framed_multiline` 同风格；全设置页单行输入统一用它）。
///
/// 白卡片上原生 TextEdit 描边几乎看不见（2026-09-12 用户实机反馈），且各处
/// 宽高不一，故收敛到这一个入口：高度与右控件区统一（`control_h`）、圆角 6。
/// 文本从左往右显示、超长时右侧截断（`clip_text`，不跟随光标滚动——长路径
/// 或长 URL 显示开头、截掉尾巴，不会把框撑开）。
fn framed_singleline(
    ui: &mut egui::Ui,
    pal: &Palette,
    text: &mut String,
    width: f32,
    hint: Option<&str>,
    password: bool,
) -> egui::Response {
    let h = control_h(ui);
    egui::Frame::new()
        .fill(pal.control_bg)
        .stroke(egui::Stroke::new(1.0, pal.card_stroke))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            let mut edit = egui::TextEdit::singleline(text)
                .frame(egui::Frame::NONE)
                .clip_text(true);
            if let Some(h) = hint {
                edit = edit.hint_text(h);
            }
            if password {
                edit = edit.password(true);
            }
            // Frame 内边距 2×2 + 描边 2：内容高 = control_h - 6，总高 = control_h
            ui.add_sized([width, h - 6.0], edit)
        })
        .inner
}

/// 带边框的多行输入框（苹果风：浅底 + 细描边 + 圆角）。
///
/// 白卡片上原生 TextEdit 描边几乎看不见，标题与输入分不开
/// （2026-09-12 用户实机反馈），故统一用 Frame 包一层。
///
/// 高度固定为视口高（`height`，不含边框内边距），内容超出时在框内滚动编辑，
/// 不再随行数把框撑高（2026-09-12 用户实机反馈："输入很长文字后文本框不应
/// 变得很高，用户可在里面滚动文字并编辑"）。
/// `id_salt` 必须每个框唯一：ScrollArea 默认 salt 相同会让多个框共享滚动
/// 状态（滚一个所有框同步滚——第二轮实机反馈）。
fn framed_multiline(
    ui: &mut egui::Ui,
    pal: &Palette,
    text: &mut String,
    height: f32,
    id_salt: &str,
) -> egui::Response {
    let fr = egui::Frame::new()
        .fill(pal.control_bg)
        .stroke(egui::Stroke::new(1.0, pal.card_stroke))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::same(6))
        .show(ui, |ui| {
            let mut resp: Option<egui::Response> = None;
            egui::ScrollArea::vertical()
                .id_salt(id_salt)
                .max_height(height)
                .min_scrolled_height(height)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    resp = Some(ui.add(
                        egui::TextEdit::multiline(text)
                            .font(egui::FontId::monospace(12.0))
                            .frame(egui::Frame::NONE)
                            .desired_width(f32::INFINITY),
                    ));
                });
            resp.unwrap_or_else(|| ui.allocate_response(egui::vec2(0.0, height), egui::Sense::hover()))
        });
    // 鼠标在框内：吞掉内层 ScrollArea 未消费的滚轮（滚到尽头也不带动设置页
    // 外层滚动）；鼠标不在框上开始滚动时照旧冒泡（2026-09-12 用户要求）。
    if ui.rect_contains_pointer(fr.response.rect) {
        ui.ctx().input_mut(|i| i.smooth_scroll_delta = egui::Vec2::ZERO);
    }
    fr.inner
}

/// JSON 文本转编辑用美化格式（合法美化，非法原样带入让用户修）。
fn pretty_json_or_raw(raw: &str) -> String {
    serde_json::from_str::<serde_json::Value>(raw)
        .map(|v| serde_json::to_string_pretty(&v).unwrap_or_else(|_| raw.to_string()))
        .unwrap_or_else(|_| raw.to_string())
}

/// JSON 编辑缓冲结算（纯函数）：合法 JSON 对象则写入配置并返回 `true`。
fn settle_params_json(draft: &mut Config, buf: &str) -> bool {
    match serde_json::from_str::<serde_json::Value>(buf) {
        Ok(v) if v.is_object() => {
            draft.translate.params_json = buf.trim().to_owned();
            true
        }
        _ => false,
    }
}

/// 卡片六：大模型参数（单 JSON 输入 + 启用自定义参数总开关 + 重置）。
///
/// 每次调用大模型时把 JSON 逐键合并进请求体（温度/上限/上下文/思考四件套等
/// 全由用户自配；总开关关闭时剔掉思考四键）。
/// 编辑规则（2026-09-12 用户要求）：敲的过程中非法只红字提示、不保存；**合法
/// 立即写入编辑缓冲（实时保存模型）**——切页/点别处/关窗都不会丢（此前只在
/// 失焦帧结算，切页那一帧卡片已不渲染，编辑内容永远进不了正式配置，用户两次
/// 反馈"格式正确却没保存成功"）。失焦时再把缓冲美化/非法回滚。
fn draw_params_card(
    ui: &mut egui::Ui,
    pal: &Palette,
    draft: &mut Config,
    changed: &mut bool,
    ed: &mut ParamsEditUi,
) {
    card(ui, pal, |ui| {
        // 标题行（说明文字已删，干净）
        ui.add_space(4.0);
        ui.label(egui::RichText::new("大模型参数").size(13.0));
        ui.add_space(4.0);
        // 总开关行：图标紧跟标签右侧，开关在右控件区
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("启用自定义参数").size(13.0));
            hint_icon(ui, THINK_TIP);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                *changed |= toggle(ui, pal, &mut draft.translate.disable_thinking);
            });
        });
        ui.add_space(4.0);
        row_separator(ui, pal);
        // 参数 JSON 输入（会话缓冲 + 失焦结算；框内恒为美化格式，见图 8）
        ui.add_space(4.0);
        let buf = ed
            .buf
            .get_or_insert_with(|| pretty_json_or_raw(&draft.translate.params_json));
        let resp = framed_multiline(ui, pal, buf, 84.0, "llm_params_json");
        let focused = resp.has_focus();
        if resp.changed() {
            // 敲的过程中：合法立即写入 draft（实时保存，切页/关窗都不丢），
            // 非法只红字提示、draft 保留上一版合法值
            if settle_params_json(draft, buf) {
                ed.err = None;
                ed.err_until = None;
                *changed = true;
            } else {
                ed.err = Some(String::from("不是合法的 JSON 对象，请检查括号/引号/逗号"));
                ed.err_until = None;
            }
        }
        if ed.had_focus && !focused {
            // 失焦结算：合法美化落盘，非法恢复上一版 + 3 秒提示
            match serde_json::from_str::<serde_json::Value>(buf) {
                Ok(v) if v.is_object() => {
                    let pretty =
                        serde_json::to_string_pretty(&v).unwrap_or_else(|_| buf.clone());
                    *buf = pretty.clone();
                    draft.translate.params_json = pretty;
                    *changed = true;
                    ed.err = None;
                    ed.err_until = None;
                }
                _ => {
                    *buf = pretty_json_or_raw(&draft.translate.params_json);
                    ed.err = Some(String::from("格式有误，已恢复上一版"));
                    ed.err_until =
                        Some(std::time::Instant::now() + std::time::Duration::from_secs(3));
                }
            }
        }
        ed.had_focus = focused;
        // 提示行：3 秒提示按时消失（过期清掉，需宿主持续重绘，见 redraw）
        if let Some(until) = ed.err_until {
            if std::time::Instant::now() >= until {
                ed.err = None;
                ed.err_until = None;
            }
        }
        if let Some(err) = ed.err.clone() {
            ui.label(
                egui::RichText::new(err)
                    .size(12.0)
                    .color(egui::Color32::from_rgb(255, 69, 58)),
            );
        }
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("未知字段服务端一般忽略。")
                .size(12.0)
                .color(pal.secondary),
        );
        ui.add_space(4.0);
        // 重置放底部（与提示词"恢复默认提示词"同逻辑）
        if primary_button(ui, pal, "重置参数").clicked() {
            draft.translate.params_json = String::from(DEFAULT_LLM_PARAMS_JSON);
            ed.buf = Some(pretty_json_or_raw(&draft.translate.params_json));
            ed.err = None;
            ed.err_until = None;
            *changed = true;
        }
        ui.add_space(4.0);
    });
}

/// 把配置里的热键字符串转成用户友好的显示（`Super` → `Win`）。
fn display_hotkey(s: &str) -> String {
    s.replace("Super", "Win")
}

/// 判断键码是否为修饰键（录制热键时忽略，修饰状态由 [`ModifiersState`] 提供）。
fn is_modifier_code(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::ControlLeft
            | KeyCode::ControlRight
            | KeyCode::AltLeft
            | KeyCode::AltRight
            | KeyCode::ShiftLeft
            | KeyCode::ShiftRight
            | KeyCode::SuperLeft
            | KeyCode::SuperRight
    )
}

/// 把 winit 键码映射为 global-hotkey 可解析的键名（`parse_hotkey` 支持的别名）。
fn keycode_to_str(code: KeyCode) -> Option<&'static str> {
    use KeyCode::*;
    Some(match code {
        KeyA => "A",
        KeyB => "B",
        KeyC => "C",
        KeyD => "D",
        KeyE => "E",
        KeyF => "F",
        KeyG => "G",
        KeyH => "H",
        KeyI => "I",
        KeyJ => "J",
        KeyK => "K",
        KeyL => "L",
        KeyM => "M",
        KeyN => "N",
        KeyO => "O",
        KeyP => "P",
        KeyQ => "Q",
        KeyR => "R",
        KeyS => "S",
        KeyT => "T",
        KeyU => "U",
        KeyV => "V",
        KeyW => "W",
        KeyX => "X",
        KeyY => "Y",
        KeyZ => "Z",
        Digit0 => "0",
        Digit1 => "1",
        Digit2 => "2",
        Digit3 => "3",
        Digit4 => "4",
        Digit5 => "5",
        Digit6 => "6",
        Digit7 => "7",
        Digit8 => "8",
        Digit9 => "9",
        F1 => "F1",
        F2 => "F2",
        F3 => "F3",
        F4 => "F4",
        F5 => "F5",
        F6 => "F6",
        F7 => "F7",
        F8 => "F8",
        F9 => "F9",
        F10 => "F10",
        F11 => "F11",
        F12 => "F12",
        Space => "Space",
        Enter => "Enter",
        Backspace => "Backspace",
        Delete => "Delete",
        Tab => "Tab",
        Home => "Home",
        End => "End",
        PageUp => "PageUp",
        PageDown => "PageDown",
        Insert => "Insert",
        ArrowUp => "Up",
        ArrowDown => "Down",
        ArrowLeft => "Left",
        ArrowRight => "Right",
        Minus => "-",
        Equal => "=",
        BracketLeft => "[",
        BracketRight => "]",
        Backslash => "\\",
        Semicolon => ";",
        Quote => "'",
        Comma => ",",
        Period => ".",
        Slash => "/",
        Backquote => "`",
        PrintScreen => "PrintScreen",
        _ => return None,
    })
}

/// 由修饰键状态 + 主键名生成热键字符串（global-hotkey 可解析格式）。
fn build_hotkey_str(mods: ModifiersState, key: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if mods.control_key() {
        parts.push("Ctrl");
    }
    if mods.alt_key() {
        parts.push("Alt");
    }
    if mods.shift_key() {
        parts.push("Shift");
    }
    if mods.super_key() {
        parts.push("Super");
    }
    parts.push(key);
    parts.join("+")
}

/// 判断某键是否允许作为无修饰键的单键热键（功能键 F1-F24、PrintScreen 等）。
fn is_standalone_ok(key: &str) -> bool {
    if let Some(rest) = key.strip_prefix('F') {
        return rest.parse::<u8>().is_ok();
    }
    matches!(key, "PrintScreen" | "ScrollLock" | "Pause")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 跑一帧 egui（无窗口布局计算）并返回闭包结果。
    fn run_ui<R>(f: impl FnOnce(&mut egui::Ui) -> R) -> R {
        let ctx = egui::Context::default();
        let mut result: Option<R> = None;
        let mut f = Some(f);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let _ = ctx
            .run_ui(input, |ui| {
                if let Some(f) = f.take() {
                    result = Some(f(ui));
                }
            })
            .drop_without_applying_deltas();
        result.expect("run_ui 闭包应执行")
    }

    /// 模拟 `setting_row` 的右侧控件位（right_to_left 水平布局，与实际调用一致；
    /// 垂直布局下 segmented 的按钮会纵向堆叠，测不出真实高度）。
    fn in_right_zone<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.scope(|ui| add(ui)).inner
            })
            .inner
        })
        .inner
    }

    /// 右控件区可视高必须严格一致（键帽/分段槽/单行输入/toggle/主按钮）。
    #[test]
    fn control_heights_are_uniform() {
        let (key_h, seg_h, input_h, toggle_h, btn_h) = run_ui(|ui| {
            let pal = palette(false);
            let key_h = in_right_zone(ui, |ui| {
                ui.scope(|ui| keycap(ui, &pal, "Ctrl+Alt+A")).response.rect.height()
            });
            ui.add_space(4.0);
            let seg_h = in_right_zone(ui, |ui| {
                let mut v = 0usize;
                ui.scope(|ui| {
                    segmented(ui, &pal, &[("a", 0usize), ("b", 1usize)], 56.0, &mut v);
                })
                .response
                .rect
                .height()
            });
            ui.add_space(4.0);
            let input_h = in_right_zone(ui, |ui| {
                let mut s = String::from("x");
                ui.scope(|ui| {
                    framed_singleline(ui, &pal, &mut s, 100.0, None, false);
                })
                .response
                .rect
                .height()
            });
            ui.add_space(4.0);
            let toggle_h = in_right_zone(ui, |ui| {
                let mut on = false;
                ui.scope(|ui| {
                    toggle(ui, &pal, &mut on);
                })
                .response
                .rect
                .height()
            });
            ui.add_space(4.0);
            let btn_h = in_right_zone(ui, |ui| primary_button(ui, &pal, "重新录制").rect.height());
            (key_h, seg_h, input_h, toggle_h, btn_h)
        });
        for (name, h) in [
            ("segmented", seg_h),
            ("framed_singleline", input_h),
            ("toggle", toggle_h),
            ("primary_button", btn_h),
        ] {
            assert!(
                (h - key_h).abs() < 0.5,
                "右控件高度不一致：键帽 {key_h} vs {name} {h}"
            );
        }
    }

    /// 多行输入框固定高度：短内容/长内容都不改变框高（长内容框内滚动）。
    #[test]
    fn framed_multiline_keeps_fixed_height_for_long_text() {
        let (short_h, long_h) = run_ui(|ui| {
            let pal = palette(false);
            let mut short = String::from("a");
            let mut long = "line\n".repeat(80);
            let a = ui
                .scope(|ui| {
                    framed_multiline(ui, &pal, &mut short, 84.0, "test_short");
                })
                .response
                .rect
                .height();
            let b = ui
                .scope(|ui| {
                    framed_multiline(ui, &pal, &mut long, 84.0, "test_long");
                })
                .response
                .rect
                .height();
            (a, b)
        });
        // 视口 84 + Frame 内边距 6×2 + 描边 2 = 98
        assert!((short_h - 98.0).abs() < 1.0, "短内容应固定 98，实际 {short_h}");
        assert!((long_h - 98.0).abs() < 1.0, "长内容不应撑高，实际 {long_h}");
    }

    /// 保存目录框（短路径）与上下 2 档 segmented 可视外宽一致。
    #[test]
    fn save_dir_input_matches_segmented_width() {
        let (seg_w, input_w) = run_ui(|ui| {
            let pal = palette(false);
            let seg_w = in_right_zone(ui, |ui| {
                let mut v = 0usize;
                ui.scope(|ui| {
                    segmented(ui, &pal, &[("a", 0usize), ("b", 1usize)], 56.0, &mut v);
                })
                .response
                .rect
                .width()
            });
            ui.add_space(4.0);
            let mut s = String::from("D:\\D1");
            let input_w = ui
                .scope(|ui| {
                    framed_singleline(
                        ui,
                        &pal,
                        &mut s,
                        segmented_width(2, 56.0) - 14.0,
                        None,
                        false,
                    );
                })
                .response
                .rect
                .width();
            (seg_w, input_w)
        });
        assert!(
            (seg_w - input_w).abs() < 0.5,
            "保存目录框外宽 {input_w} 与分段槽 {seg_w} 不等"
        );
    }

    /// JSON 编辑缓冲结算：只接受合法 JSON 对象，非法/数组不写入。
    #[test]
    fn settle_params_json_only_accepts_object() {
        let mut cfg = Config::default();
        cfg.translate.params_json = String::from("{\"a\":1}");
        assert!(settle_params_json(&mut cfg, " {\"b\": 2} "));
        assert_eq!(cfg.translate.params_json, "{\"b\": 2}");
        assert!(!settle_params_json(&mut cfg, "[1,2]"));
        assert!(!settle_params_json(&mut cfg, "{bad"));
        assert_eq!(cfg.translate.params_json, "{\"b\": 2}");
    }
}
