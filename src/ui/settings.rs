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

use anyhow::Context;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::config::{Config, SaveFormat, SaveMode};

use super::gui::GuiState;

/// 设置窗口默认大小（逻辑像素）。
const DEFAULT_SIZE: (u32, u32) = (620, 600);

/// 设置主界面窗口。
pub struct Settings {
    window: Arc<Window>,
    gui: GuiState,
    /// 编辑缓冲（打开时从当前配置克隆，保存成功前不改动正式配置）。
    draft: Config,
    /// 保存目录的字符串编辑缓冲（`PathBuf` 不便直接进文本框）。
    dir_input: String,
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
            recording_hotkey: false,
            modifiers: ModifiersState::empty(),
            pending_save: false,
            close_requested: false,
            status: None,
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
        // clone 到局部变量，避免闭包与 self.gui 的双重可变借用
        let mut draft = self.draft.clone();
        let mut dir_input = self.dir_input.clone();
        let mut changed = false;
        let mut start_recording = false;
        let recording = self.recording_hotkey;
        let status = self.status.clone();

        self.gui.render(self.window.as_ref(), |ui| {
            draw_settings_ui(
                ui,
                &mut draft,
                &mut dir_input,
                recording,
                &mut changed,
                &mut start_recording,
            );
            if let Some((ok, msg)) = &status {
                let color = if *ok {
                    egui::Color32::from_rgb(80, 200, 120)
                } else {
                    egui::Color32::from_rgb(255, 90, 90)
                };
                ui.colored_label(color, msg);
            }
        });

        // 闭包同步执行完毕，把编辑结果写回
        self.draft = draft;
        self.dir_input = dir_input;
        if start_recording {
            self.recording_hotkey = true;
        }
        if changed {
            self.pending_save = true;
        }
    }
}

/// 绘制全部设置项（在 `redraw` 闭包内调用，编辑局部 `draft`/`dir_input`）。
fn draw_settings_ui(
    ui: &mut egui::Ui,
    draft: &mut Config,
    dir_input: &mut String,
    recording: bool,
    changed: &mut bool,
    start_recording: &mut bool,
) {
    ui.vertical_centered(|ui| {
        ui.label(egui::RichText::new("PrismaSnap 设置").size(20.0).strong());
    });
    ui.add_space(6.0);

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new("settings_main")
                .num_columns(2)
                .spacing([24.0, 12.0])
                .min_col_width(96.0)
                .show(ui, |ui| {
                    // ── 全局热键 ──
                    section_title(ui, "全局热键");
                    ui.end_row();

                    ui.label("截图热键");
                    ui.horizontal(|ui| {
                        if recording {
                            ui.label(
                                egui::RichText::new("请按下组合键（Esc 取消）")
                                    .size(14.0)
                                    .color(egui::Color32::from_rgb(0, 160, 255)),
                            );
                        } else {
                            ui.label(egui::RichText::new(display_hotkey(&draft.hotkey)).size(14.0));
                            if ui
                                .add_sized(
                                    [72.0, 26.0],
                                    egui::Button::new(
                                        egui::RichText::new("录制").size(14.0),
                                    ),
                                )
                                .clicked()
                            {
                                *start_recording = true;
                            }
                        }
                    });
                    ui.end_row();

                    // ── 保存行为 ──
                    ui.add_space(4.0);
                    ui.end_row();
                    section_title(ui, "保存行为");
                    ui.end_row();

                    ui.label("保存模式");
                    ui.horizontal(|ui| {
                        *changed |= ui
                            .radio_value(&mut draft.save.mode, SaveMode::Silent, "静默保存")
                            .changed();
                        *changed |= ui
                            .radio_value(&mut draft.save.mode, SaveMode::AlwaysAsk, "每次询问")
                            .changed();
                    });
                    ui.end_row();

                    ui.label("保存目录");
                    *changed |= ui.text_edit_singleline(dir_input).changed();
                    ui.end_row();

                    ui.label("图片格式");
                    *changed |= egui::ComboBox::from_id_salt("save_format")
                        .width(90.0)
                        .selected_text(format_name(draft.save.format))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut draft.save.format, SaveFormat::Png, "PNG");
                            ui.selectable_value(&mut draft.save.format, SaveFormat::Jpeg, "JPEG");
                        })
                        .response
                        .changed();
                    ui.end_row();

                    if draft.save.format == SaveFormat::Jpeg {
                        ui.label("JPEG 质量");
                        *changed |=
                            ui.add(egui::Slider::new(&mut draft.save.jpeg_quality, 1..=100))
                                .changed();
                        ui.end_row();
                    }

                    ui.label("");
                    ui.label(
                        egui::RichText::new("目录留空 = <程序目录>/screenshots")
                            .size(12.0)
                            .color(egui::Color32::GRAY),
                    );
                    ui.end_row();

                    // ── 捕获 ──
                    ui.add_space(4.0);
                    ui.end_row();
                    section_title(ui, "捕获");
                    ui.end_row();

                    ui.label("光标");
                    *changed |= ui
                        .checkbox(&mut draft.capture.cursor_visible, "截图包含鼠标光标")
                        .changed();
                    ui.end_row();

                    // ── AI 接口 ──
                    ui.add_space(4.0);
                    ui.end_row();
                    section_title(ui, "AI 接口（OpenAI 兼容）");
                    ui.end_row();

                    ui.label("API 地址");
                    *changed |= ui.text_edit_singleline(&mut draft.llm.api_url).changed();
                    ui.end_row();

                    ui.label("API Key");
                    *changed |= ui
                        .add(egui::TextEdit::singleline(&mut draft.llm.api_key).password(true))
                        .changed();
                    ui.end_row();

                    ui.label("模型");
                    *changed |= ui.text_edit_singleline(&mut draft.llm.model).changed();
                    ui.end_row();

                    ui.label("翻译目标");
                    *changed |= ui
                        .text_edit_singleline(&mut draft.llm.translate_target)
                        .changed();
                    ui.end_row();
                });

            ui.add_space(8.0);
        });
}

/// 分组标题（统一样式，跨两列占位）。
fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).size(16.0).strong());
    ui.label("");
}

/// `SaveFormat` 的展示名。
fn format_name(f: SaveFormat) -> &'static str {
    match f {
        SaveFormat::Png => "PNG",
        SaveFormat::Jpeg => "JPEG",
    }
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
