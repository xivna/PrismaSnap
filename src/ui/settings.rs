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
    TranslateMode, TranslatePrompts,
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
    pub fn create_window(event_loop: &ActiveEventLoop) -> anyhow::Result<Arc<Window>> {
        let attrs = Window::default_attributes()
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
        event_loop
            .create_window(attrs)
            .context("创建设置窗口失败")
            .map(Arc::new)
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
        })
    }

    /// 窗口 id（宿主按 id 分发事件）。
    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    /// 隐藏窗口（截图期间避免遮挡）。
    pub fn hide(&self) {
        self.window.set_visible(false);
    }

    /// 显示窗口并聚焦（截图结束后恢复）。
    pub fn show(&self) {
        self.window.set_visible(true);
        self.window.focus_window();
        self.window.request_redraw();
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

    /// 把目录文本缓冲同步回编辑缓冲并返回其引用（保存前调用）。
    pub fn apply_draft(&mut self) -> &Config {
        self.draft.save.dir = PathBuf::from(self.dir_input.trim());
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
            );
        });

        // 闭包同步执行完毕，把编辑结果写回
        self.draft = draft;
        self.dir_input = dir_input;
        self.active_section = active_section;
        self.model_dl = mdl;
        self.font_pickers = font_pickers;
        if start_recording {
            self.recording_hotkey = true;
        }
        if changed {
            self.pending_save = true;
        }
        // 下载进行中时持续重绘，进度百分比才动
        if self.model_dl.any_live() {
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
                Section::Ai => draw_ai(ui, &pal, draft, changed, mdl),
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
                keycap(ui, pal, &display_hotkey(&draft.hotkey));
                ui.add_space(8.0);
                if primary_button(ui, pal, "重新录制").clicked() {
                    *start_recording = true;
                }
            }
        });
        row_separator(ui, pal);
        setting_row(ui, "日志级别（需重启）", |ui| {
            *changed |= segmented(
                ui,
                pal,
                &[
                    ("错误", LogLevel::Error),
                    ("警告", LogLevel::Warn),
                    ("信息", LogLevel::Info),
                    ("调试", LogLevel::Debug),
                    ("详细", LogLevel::Trace),
                ],
                52.0,
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
            *changed |= ui
                .add_sized(
                    [220.0, 24.0],
                    egui::TextEdit::singleline(dir_input)
                        .hint_text("<程序目录>/screenshots"),
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
                *changed |= ui
                    .add_sized(
                        [160.0, 24.0],
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
        120.0,
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
) {
    // 卡片一：基础 LLM（即文本翻译后端；行布局保持不动，只改绑定到新配置）。
    card(ui, pal, |ui| {
        setting_row(ui, "API 地址", |ui| {
            *changed |= ui
                .add_sized(
                    [240.0, 24.0],
                    egui::TextEdit::singleline(&mut draft.translate.text_llm.api_url)
                        .hint_text("http://127.0.0.1:8080/v1/chat/completions"),
                )
                .changed();
        });
        row_separator(ui, pal);
        setting_row(ui, "API Key", |ui| {
            *changed |= ui
                .add_sized(
                    [240.0, 24.0],
                    egui::TextEdit::singleline(&mut draft.translate.text_llm.api_key).password(true),
                )
                .changed();
        });
        row_separator(ui, pal);
        setting_row(ui, "模型", |ui| {
            *changed |= ui
                .add_sized(
                    [240.0, 24.0],
                    egui::TextEdit::singleline(&mut draft.translate.text_llm.model),
                )
                .changed();
        });
        row_separator(ui, pal);
        setting_row(ui, "翻译目标", |ui| {
            *changed |= ui
                .add_sized(
                    [240.0, 24.0],
                    egui::TextEdit::singleline(&mut draft.translate.target_lang)
                        .hint_text("简体中文"),
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
                *changed |= ui
                    .add_sized(
                        [240.0, 24.0],
                        egui::TextEdit::singleline(&mut draft.translate.multimodal_llm.api_url)
                            .hint_text("多模态模型地址"),
                    )
                    .changed();
            });
            row_separator(ui, pal);
            setting_row(ui, "API Key", |ui| {
                *changed |= ui
                    .add_sized(
                        [240.0, 24.0],
                        egui::TextEdit::singleline(&mut draft.translate.multimodal_llm.api_key)
                            .password(true),
                    )
                    .changed();
            });
            row_separator(ui, pal);
            setting_row(ui, "模型", |ui| {
                *changed |= ui
                    .add_sized(
                        [240.0, 24.0],
                        egui::TextEdit::singleline(&mut draft.translate.multimodal_llm.model)
                            .hint_text("如 qwen-vl-max"),
                    )
                    .changed();
            });
        }
    });
    ui.add_space(12.0);

    // 卡片三：翻译模式（三选一，默认自动；Auto 才显示阈值滑块；
    // "?" 按钮点出悬浮窗介绍各选项含义）。
    card(ui, pal, |ui| {
        setting_row(ui, "翻译模式", |ui| {
            // 右对齐行内先加 "?"（最右侧），再加分段按钮组（其左侧）。
            let tip = ui.small_button("?");
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
            egui::Popup::menu(&tip).show(|ui| {
                ui.set_width(300.0);
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
        if draft.translate.mode == TranslateMode::Auto {
            row_separator(ui, pal);
            setting_row(ui, "置信度阈值", |ui| {
                *changed |= ui
                    .add_sized(
                        [160.0, 24.0],
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

    // 卡片六：提示词模板（纯文本/多模态单图/多模态批量；留空用内置默认）。
    card(ui, pal, |ui| {
        ui.add_space(4.0);
        ui.label(egui::RichText::new("提示词模板").size(13.0));
        ui.label(
            egui::RichText::new("占位符：{target} 目标语言，{items} 输入条目（纯文本），{count} 图片数（批量）。留空即用内置默认。")
                .size(12.0)
                .color(pal.secondary),
        );
        ui.add_space(4.0);
        for (label, field) in [
            ("纯文本", &mut draft.translate.prompts.text),
            ("多模态单图", &mut draft.translate.prompts.multimodal_single),
            ("多模态批量", &mut draft.translate.prompts.multimodal_batch),
        ] {
            ui.label(egui::RichText::new(label).size(12.5).strong());
            *changed |= ui
                .add_sized(
                    [ui.available_width(), 84.0],
                    egui::TextEdit::multiline(field).font(egui::FontId::monospace(12.0)),
                )
                .changed();
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
        // 标题行：左"OCR插件下载"，右 取消（有任务才显示）+ 来源 + 帮助
        setting_row(ui, "OCR插件下载", |ui| {
            ui.horizontal(|ui| {
                if mdl.any_job() && ui.small_button("取消").clicked() {
                    mdl.cancel_all();
                }
                ui.label(
                    egui::RichText::new("官方自动下载").size(12.0).color(pal.secondary),
                );
                if ui.small_button("手动下载帮助").clicked() {
                    mdl.show_help = true;
                    mdl.copied = None;
                }
            });
        });
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
        draw_model_help(ui.ctx(), mdl);
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
fn draw_model_help(ctx: &egui::Context, mdl: &mut ModelDownloadUi) {
    let mut open = mdl.show_help;
    egui::Window::new("手动下载帮助")
        .open(&mut open)
        .resizable(true)
        .default_width(520.0)
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
    egui::Frame::new()
        .fill(pal.control_bg)
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
                if ui.add_sized(egui::vec2(button_width, 20.0), btn).clicked() {
                    *value = *v;
                    changed = true;
                }
            }
        });
    changed
}

/// 键帽徽章（展示当前热键，弱控件底色 + 细描边）。
fn keycap(ui: &mut egui::Ui, pal: &Palette, text: &str) {
    egui::Frame::new()
        .fill(pal.control_bg)
        .stroke(egui::Stroke::new(1.0, pal.card_stroke))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).size(12.5).strong());
        });
}

/// 强调色主按钮（白字 + accent 底）。
fn primary_button(ui: &mut egui::Ui, pal: &Palette, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(text).size(12.5).color(egui::Color32::WHITE))
            .fill(pal.accent)
            .stroke(egui::Stroke::NONE)
            .corner_radius(6.0)
            .min_size(egui::vec2(56.0, 24.0)),
    )
}

/// iOS 风格开关，返回是否变更。
fn toggle(ui: &mut egui::Ui, pal: &Palette, on: &mut bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(40.0, 24.0), egui::Sense::click());
    let mut changed = false;
    if response.clicked() {
        *on = !*on;
        changed = true;
    }
    let anim = ui.ctx().animate_bool_with_time(response.id, *on, 0.15);
    let track = pal.control_bg.lerp_to_gamma(pal.accent, anim);
    ui.painter().rect_filled(rect, 12.0, track);
    let knob_x = egui::emath::lerp(rect.left() + 12.0..=rect.right() - 12.0, anim);
    ui.painter().circle_filled(egui::pos2(knob_x, rect.center().y), 9.0, egui::Color32::WHITE);
    response.on_hover_cursor(egui::CursorIcon::PointingHand);
    changed
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
