//! PrismaSnap 程序入口。
//!
//! 启动流程：单实例检测 → 日志 → 配置 → 全局热键 → 托盘 → winit 事件循环。
//!
//! 主循环架构（路线 A，见 PROGRESS.md 决策记录）：
//! **winit `EventLoop` 接管消息循环**（底层即 Windows 消息泵），托盘菜单事件
//! 在 `about_to_wait` 轮询 channel；全局热键检测走 winit 的 `with_msg_hook`
//! 消息钩子（WM_HOTKEY 投递到线程消息队列，钩子拦截置位原子标志，主循环
//! 在 `about_to_wait` 消费——延续"消息级自检测"的既有决策）。
//!
//! 截图流程：热键/托盘触发 → 独立线程「定位 → 捕获 → HDR 转换」→
//! `EventLoopProxy` 发 `CaptureDone` 唤醒主循环 → 创建覆盖层窗口选区 →
//! Esc 取消 / Enter·Ctrl+C 复制 / Ctrl+S 保存（AGENTS.md 2.1 节）。
//!
//! 设置主界面：托盘「打开设置」菜单项或双击托盘图标打开；编辑缓冲保存后
//! 热键即时改绑、配置写盘热更新。截图期间隐藏设置窗口、结束后恢复（避免
//! 遮挡与抢焦点）。
//!
//! 发布版无控制台窗口（GUI 程序），日志写入 exe 同目录 logs/；
//! 调试需要控制台时用 `--features console` 编译。

// 无控制台窗口（除非启用 console feature 调试）
#![cfg_attr(
    all(target_os = "windows", not(feature = "console")),
    windows_subsystem = "windows"
)]

#[cfg(target_os = "windows")]
mod imp {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;

    use anyhow::Context;
    use tracing::{error, info, warn};
    use winit::application::ApplicationHandler;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
    use winit::platform::windows::EventLoopBuilderExtWindows;
    use winit::window::WindowId;

    use prismsnap::capture::{display_info, engine, frame};
    use prismsnap::config::Config;
    use prismsnap::hotkey::HotkeyManager;
        use prismsnap::ui::ai::AiDone;
        use prismsnap::ui::overlay::{CapturedShot, Overlay};
    use prismsnap::ui::settings::Settings;
    use prismsnap::ui::tray::{Tray, TrayAction};
    use prismsnap::utils::{fontsel, logging, paths, single_instance};

    /// winit 自定义事件：捕获线程完成截图后经 `EventLoopProxy` 唤醒主循环。
    enum UserEvent {
        CaptureDone(Result<CapturedShot, String>),
        /// AI 后台任务（OCR/翻译）完成，附带请求序号供覆盖层丢弃过期结果。
        Ai(AiDone),
    }

    /// 读取当前前台窗口句柄（截图触发时调用；失败返回 `None`，不影响流程）。
    fn foreground_window() -> Option<isize> {
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        // SAFETY：纯查询调用，无资源管理；返回空句柄视为无前台窗口。
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.is_invalid() {
            None
        } else {
            Some(hwnd.0 as isize)
        }
    }

    /// 把焦点归还给截图前的窗口（best-effort：系统可能因前台锁拒绝）。
    fn restore_foreground_window(hwnd_raw: isize) {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
        // SAFETY：句柄来自本进程稍早时刻的 GetForegroundWindow；窗口已关闭时
        // 调用失败，仅记日志，不影响退出流程。
        let ok = unsafe { SetForegroundWindow(HWND(hwnd_raw as *mut _)) };
        if ok.as_bool() {
            info!("已恢复截图前窗口焦点");
        } else {
            warn!("恢复截图前窗口焦点被系统拒绝（前台锁），保持当前焦点");
        }
    }

    /// 应用状态：托盘 + 热键 + 覆盖层窗口 + 设置窗口。
    struct App {
        config: Arc<Config>,
        /// 配置文件路径（设置保存时写回）。
        config_path: PathBuf,
        tray: Tray,
        /// 热键管理器（设置界面改绑时 `rebind`）。
        hotkeys: HotkeyManager,
        /// 当前已注册热键 id（`Arc<AtomicU32>` 供消息钩子实时读取，改绑后更新）。
        hotkey_id: Arc<AtomicU32>,
        /// 消息钩子检测到热键时置位，主循环在 `about_to_wait` 里消费。
        hotkey_triggered: Arc<AtomicBool>,
        /// 捕获线程回传结果的通道（克隆进线程）。
        proxy: EventLoopProxy<UserEvent>,
        /// 当前覆盖层窗口（None 表示无截图会话）。
        overlay: Option<Overlay>,
        /// 设置主界面窗口（None 表示未打开）。
        settings: Option<Settings>,
        /// 热键是否因录制被挂起（录制结束恢复）。
        hotkey_suspended: bool,
        /// 捕获进行中（防止热键连按重复触发）。
        capturing: bool,
        /// 截图触发前的前台窗口句柄（`GetForegroundWindow` 原始值，截图结束
        /// 后 `SetForegroundWindow` 归还焦点，见 AGENTS.md 3.9 节遗留项）。
        prev_foreground: Option<isize>,
    }

    impl App {
        /// 触发一次截图：起独立线程执行捕获链路，完成经 proxy 回传。
        fn trigger_capture(&mut self) {
            if self.capturing || self.overlay.is_some() {
                return;
            }
            self.capturing = true;
            info!("触发截图");
            // 记住当前前台窗口：覆盖层需要键盘焦点（Esc/Enter），截图期间焦点
            // 会转移；结束后在 close_overlay 里归还（失败只记日志，不影响流程）。
            self.prev_foreground = foreground_window();
            // 2026-09-12 用户要求：设置窗口不再响应截图动作——触发时不隐藏，
            // 结束时不恢复，保持用户自己安排的可见状态。
            let config = self.config.clone();
            let proxy = self.proxy.clone();
            std::thread::spawn(move || {
                let result = capture_shot(&config);
                let _ = proxy.send_event(UserEvent::CaptureDone(result));
            });
        }

        /// 打开覆盖层窗口进入选区流程。
        fn open_overlay(&mut self, event_loop: &ActiveEventLoop, shot: CapturedShot) {
            let window = match Overlay::create_window(event_loop, &shot.monitor_rect) {
                Ok(w) => w,
                Err(e) => {
                    error!("创建覆盖层窗口失败: {e:#}");
                    return;
                }
            };
            let mut overlay = match Overlay::new(window.clone(), shot, self.config.clone()) {
                Ok(o) => o,
                Err(e) => {
                    error!("初始化覆盖层失败: {e:#}");
                    return;
                }
            };
            // AI 后台任务经 EventLoopProxy 回主循环（AGENTS.md 3.10 节路线 A）
            {
                let proxy = self.proxy.clone();
                overlay.set_ai_notify(move |done| {
                    let _ = proxy.send_event(UserEvent::Ai(done));
                });
            }
            // 隐藏状态下尽量把首帧 present 出去。swapchain Outdated 时 render
            // 内部会 reconfigure 重试；仍失败则显示后再补一帧。
            let mut presented = false;
            for _ in 0..3 {
                presented = overlay.redraw();
                if presented {
                    break;
                }
            }
            window.set_visible(true);
            // 覆盖层需要键盘输入（Esc/Enter/Ctrl+C/Ctrl+S），显示后取焦点
            window.focus_window();
            if !presented {
                window.request_redraw();
            }
            self.overlay = Some(overlay);
        }

        /// 处理覆盖层退出请求（销毁窗口，回到等待热键状态）。
        fn close_overlay(&mut self, event_loop: &ActiveEventLoop) {
            info!("关闭覆盖层");
            self.overlay = None;
            // 归还焦点给截图前的窗口（best-effort：系统前台锁可能拒绝，只记日志）
            if let Some(hwnd) = self.prev_foreground.take() {
                restore_foreground_window(hwnd);
            }
            // 2026-09-12 用户要求：设置窗口不再响应截图动作——结束时不恢复显示，
            // 只按窗口是否还在决定事件循环模式（设置开着保持 Poll）。
            if self.settings.is_some() {
                event_loop.set_control_flow(ControlFlow::Poll);
            } else {
                event_loop.set_control_flow(ControlFlow::Wait);
            }
        }

        /// 打开设置窗口（已打开则聚焦）。
        fn open_settings(&mut self, event_loop: &ActiveEventLoop) {
            // 截图会话中不打开设置窗口，避免与全屏置顶覆盖层抢焦点
            if self.overlay.is_some() {
                return;
            }
            if let Some(settings) = &self.settings {
                settings.focus();
                return;
            }
            let window = match Settings::create_window(event_loop, self.config.ui.settings_pos) {
                Ok(w) => w,
                Err(e) => {
                    error!("创建设置窗口失败: {e:#}");
                    return;
                }
            };
            let mut settings = match Settings::new(window.clone(), &self.config) {
                Ok(s) => s,
                Err(e) => {
                    error!("初始化设置窗口失败: {e:#}");
                    return;
                }
            };
            settings.redraw();
            window.set_visible(true);
            window.focus_window();
            window.request_redraw();
            self.settings = Some(settings);
            event_loop.set_control_flow(ControlFlow::Poll);
        }

        /// 关闭设置窗口（丢弃编辑缓冲，记录窗口位置并写盘下次恢复）。
        fn close_settings(&mut self, event_loop: &ActiveEventLoop) {
            // 录制期间关闭窗口：恢复被挂起的全局热键
            if self.hotkey_suspended {
                if let Err(e) = self.hotkeys.resume() {
                    error!("恢复全局热键失败: {e:#}");
                }
                self.hotkey_suspended = false;
            }
            // 记录关闭位置：直接改生效配置并写盘（不走 draft，避免把未点保存的
            // 编辑缓冲一并落盘；位置与编辑内容是两回事）
            if let Some(s) = &self.settings {
                if let Some(pos) = s.outer_position() {
                    let mut cfg = (*self.config).clone();
                    cfg.ui.settings_pos = Some(pos);
                    match cfg.save(&self.config_path) {
                        Ok(()) => self.config = Arc::new(cfg),
                        Err(e) => error!("设置窗口位置写盘失败: {e:#}"),
                    }
                }
            }
            self.settings = None;
            if self.overlay.is_none() {
                event_loop.set_control_flow(ControlFlow::Wait);
            }
        }

        /// 消费设置窗口的保存/关闭请求（窗口事件后调用，借用独立）。
        fn handle_settings_flags(&mut self, event_loop: &ActiveEventLoop) {
            // 录制状态变化 → 挂起/恢复全局热键（避免旧热键拦截录制按键）
            self.sync_hotkey_suspension();

            let (pending, close) = match &self.settings {
                Some(s) => (s.pending_save, s.close_requested),
                None => return,
            };
            if close {
                if let Some(s) = &mut self.settings {
                    s.close_requested = false;
                }
                // 先落盘已确认的变更再关闭：JSON 参数失焦落盘与点 X 关闭同帧时，
                // 不先 apply 就会丢弃用户刚确认的内容（2026-09-12 用户实机反馈）；
                // "丢弃编辑缓冲"只丢未确认的编辑，已 pending 的变更必须保存
                if pending {
                    if let Some(s) = &mut self.settings {
                        s.pending_save = false;
                    }
                    self.apply_settings();
                }
                self.close_settings(event_loop);
                return;
            }
            if pending {
                if let Some(s) = &mut self.settings {
                    s.pending_save = false;
                }
                self.apply_settings();
            }
        }

        /// 按设置窗口的录制状态挂起/恢复全局热键。
        fn sync_hotkey_suspension(&mut self) {
            let recording = self
                .settings
                .as_ref()
                .is_some_and(|s| s.is_recording());
            if recording == self.hotkey_suspended {
                return;
            }
            if recording {
                if let Err(e) = self.hotkeys.suspend() {
                    warn!("挂起全局热键失败: {e:#}");
                } else {
                    self.hotkey_suspended = true;
                }
            } else if let Err(e) = self.hotkeys.resume() {
                error!("恢复全局热键失败: {e:#}");
            } else {
                self.hotkey_suspended = false;
            }
        }

        /// 实时保存设置：热键变更则 `rebind`，其余变更静默写盘并热更新 `App.config`。
        fn apply_settings(&mut self) {
            // 先把编辑缓冲同步出 owned 副本，释放对 settings 的借用
            let draft = match &mut self.settings {
                Some(s) => s.apply_draft().clone(),
                None => return,
            };
            // 热键变化才 rebind（先注册新热键，失败回退并提示，不写盘）
            if draft.hotkey != self.config.hotkey {
                if let Err(e) = self.hotkeys.rebind(&draft.hotkey) {
                    if let Some(s) = &mut self.settings {
                        s.set_status(false, format!("热键注册失败：{e:#}"));
                        s.revert_hotkey(&self.config.hotkey);
                    }
                    return;
                }
                self.hotkey_id.store(self.hotkeys.id(), Ordering::SeqCst);
                info!("全局热键已改绑: {}", draft.hotkey);
                if let Some(s) = &mut self.settings {
                    s.set_status(true, "热键已更新");
                }
            }
            // 写盘
            if let Err(e) = draft.save(&self.config_path) {
                if let Some(s) = &mut self.settings {
                    s.set_status(false, format!("保存失败：{e:#}"));
                }
                return;
            }
            self.config = Arc::new(draft);
            info!("配置已保存: {}", self.config_path.display());
        }
    }

    impl ApplicationHandler<UserEvent> for App {
        fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

        fn window_event(
            &mut self,
            event_loop: &ActiveEventLoop,
            window_id: WindowId,
            event: WindowEvent,
        ) {
            // 覆盖层窗口事件
            if let Some(overlay) = &mut self.overlay {
                if overlay.window_id() == window_id {
                    overlay.on_window_event(&event);
                    // 工具条字体选择同步了全局默认：写盘 + 热更新 App.config
                    // （设置菜单下次打开即显示新字体；截图期间设置窗口隐藏，无竞写）
                    if let Some(cfg) = overlay.take_pending_config() {
                        match cfg.save(&self.config_path) {
                            Ok(()) => {
                                self.config = Arc::new(cfg);
                                info!("标注字体已同步为全局默认");
                            }
                            Err(e) => error!("同步标注字体写盘失败: {e:#}"),
                        }
                    }
                    if overlay.exit_requested {
                        self.close_overlay(event_loop);
                    }
                    return;
                }
            }
            // 设置窗口事件
            if let Some(settings) = &mut self.settings {
                if settings.window_id() == window_id {
                    settings.on_window_event(&event);
                } else {
                    return;
                }
            } else {
                return;
            }
            // 处理设置窗口的保存/关闭请求（借用独立，避免与上面的可变借用冲突）
            self.handle_settings_flags(event_loop);
        }

        fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
            match event {
                UserEvent::CaptureDone(Ok(shot)) => {
                    self.capturing = false;
                    info!(
                        "截图完成: {}x{} is_hdr={}",
                        shot.img.width(),
                        shot.img.height(),
                        shot.is_hdr
                    );
                    self.open_overlay(event_loop, shot);
                    // 覆盖层期间持续轮询（egui 重绘 / 光标移动）
                    event_loop.set_control_flow(ControlFlow::Poll);
                }
                UserEvent::CaptureDone(Err(e)) => {
                    self.capturing = false;
                    error!("截图失败: {e}");
                }
                UserEvent::Ai(done) => {
                    // 覆盖层已退出时结果无处投递，直接丢弃
                    if let Some(overlay) = &mut self.overlay {
                        overlay.on_ai_done(done);
                    }
                }
            }
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            if self.hotkey_triggered.swap(false, Ordering::SeqCst) {
                self.trigger_capture();
            }
            match self.tray.poll_action() {
                Some(TrayAction::Capture) => self.trigger_capture(),
                Some(TrayAction::OpenSettings) => self.open_settings(event_loop),
                Some(TrayAction::Exit) => {
                    info!("托盘菜单退出");
                    event_loop.exit();
                }
                None => {}
            }
        }

        fn exiting(&mut self, _event_loop: &ActiveEventLoop) {}
    }

    /// 截图主链路（捕获线程内执行）：定位 → 捕获 → HDR 转换。
    fn capture_shot(config: &Config) -> Result<CapturedShot, String> {
        let monitor_rect = engine::monitor_rect_at_cursor().map_err(|e| e.to_string())?;
        let monitor = engine::monitor_at_cursor().map_err(|e| e.to_string())?;
        let device_name = engine::monitor_device_name(&monitor);
        // 捕获前先查 HDR：SDR 走 Rgba8，避免 WGC 16F 触发 DWM 格式切换闪屏。
        let is_hdr = match display_info::query_is_hdr(&device_name) {
            Ok(h) => h,
            Err(e) => {
                warn!("HDR 状态查询失败: {e}，按 SDR 屏原图直出");
                false
            }
        };
        let raw = engine::capture_frame(monitor, config.capture.cursor_visible, is_hdr)
            .map_err(|e| e.to_string())?;
        info!("捕获完成: {}x{} format={:?}", raw.width, raw.height, raw.format);
        let mut sdr_white_scrgb = 1.0f32;
        let img = if is_hdr {
            info!("显示器处于 HDR 模式，走 HDR 色彩转换");
            sdr_white_scrgb = match display_info::query_sdr_white_nits(&raw.device_name) {
                Ok(n) => {
                    info!("SDR 白点: {n} nit");
                    n / 80.0
                }
                Err(e) => {
                    warn!("SDR 白点查询失败: {e}，回退 80 nit");
                    1.0
                }
            };
            frame::frame_to_srgb_image(&raw, sdr_white_scrgb)
        } else {
            info!("显示器处于 SDR 模式，原图直出");
            match raw.format {
                frame::RawFrameFormat::Rgba8 => frame::frame_rgba8_to_image(&raw),
                frame::RawFrameFormat::Rgba16F => frame::frame_to_srgb_image_direct(&raw),
            }
        };
        Ok(CapturedShot {
            img,
            raw,
            is_hdr,
            // HDR 预览 UI 层亮度提升用（SDR 屏为 1.0，不生效）
            sdr_white_scrgb,
            monitor_rect,
        })
    }

    /// 程序入口：单实例 → 日志 → 配置 → 热键 → 托盘 → 事件循环。
    pub fn run() -> anyhow::Result<()> {
        // 1. 单实例检测
        let _instance = match single_instance::acquire("Global\\PrismaSnap-SingleInstance")? {
            Some(guard) => guard,
            None => {
                // 第二个实例：弹窗提示后退出（无控制台窗口，eprintln 用户看不到）
                use windows::Win32::UI::WindowsAndMessaging::{
                    MessageBoxW, MB_ICONINFORMATION, MB_OK,
                };
                unsafe {
                    let _ = MessageBoxW(
                        None,
                        windows::core::w!("PrismaSnap 已在运行，请查看系统托盘。"),
                        windows::core::w!("PrismaSnap"),
                        MB_OK | MB_ICONINFORMATION,
                    );
                }
                return Ok(());
            }
        };

        // 2. 日志（文件句柄须持有到进程退出；级别读自配置，改后重启生效）
        let config_path_early = Config::default_path()?;
        let log_level = Config::load_log_level(&config_path_early);
        let _log_file = logging::init(&paths::exe_dir()?.join("logs"), &log_level)?;
        info!("PrismaSnap 启动");

        // panic 兜底：把崩溃栈写进日志（发布版无控制台，不设 hook 什么都看不到）
        std::panic::set_hook(Box::new(|info| {
            error!("panic: {info}");
            eprintln!("panic: {info}");
        }));

        // 3. 配置（路径前面读日志级别时已算过，直接复用）
        let config_path = config_path_early;
        let config = match Config::load(&config_path) {
            Ok(c) => c,
            Err(e) => {
                warn!("配置文件损坏，使用默认配置: {e}");
                Config::default()
            }
        };
        info!("配置加载: {}", config_path.display());

        // 3.5 字体选择（界面字体 + 标注/翻译字体，Phase 5；空 = 系统默认）
        fontsel::set_interface_font((!config.ui.interface_font.is_empty())
            .then(|| config.ui.interface_font.clone()));
        fontsel::set_annotation_font((!config.ui.annotation_font.is_empty())
            .then(|| config.ui.annotation_font.clone()));
        if !config.ui.interface_font.is_empty() || !config.ui.annotation_font.is_empty() {
            info!(
                "字体选择: 界面={} 标注={}",
                config.ui.interface_font, config.ui.annotation_font
            );
        }

        // 4. 全局热键（在事件循环创建前注册，id 供消息钩子比对）
        let hotkeys = HotkeyManager::register(&config.hotkey).with_context(|| {
            format!("注册全局热键失败（可能被其他程序占用）: {}", config.hotkey)
        })?;
        let hotkey_id = Arc::new(AtomicU32::new(hotkeys.id()));
        info!("全局热键已注册: {} (id {})", config.hotkey, hotkey_id.load(Ordering::SeqCst));

        // 5. 托盘
        let tray = Tray::new()?;

        // 6. winit 事件循环：WM_HOTKEY 检测走消息钩子（线程消息队列级）
        let hotkey_triggered = Arc::new(AtomicBool::new(false));
        let mut builder = EventLoop::<UserEvent>::with_user_event();
        {
            let flag = hotkey_triggered.clone();
            let hotkey_id_flag = hotkey_id.clone();
            builder.with_msg_hook(move |msg| {
                use windows::Win32::UI::WindowsAndMessaging::{MSG, WM_HOTKEY};
                // Safety: winit 保证传入的指针指向有效的 MSG
                let msg = unsafe { &*(msg as *const MSG) };
                if msg.message == WM_HOTKEY
                    && msg.wParam.0 as u32 == hotkey_id_flag.load(Ordering::SeqCst)
                {
                    flag.store(true, Ordering::SeqCst);
                    // 返回 true 消费消息，避免 winit 再次分发
                    return true;
                }
                false
            });
        }
        let event_loop = builder.build().context("创建事件循环失败")?;
        event_loop.set_control_flow(ControlFlow::Wait);
        let proxy = event_loop.create_proxy();

        let mut app = App {
            config: Arc::new(config),
            config_path,
            tray,
            hotkeys,
            hotkey_id,
            hotkey_triggered,
            proxy,
            overlay: None,
            settings: None,
            hotkey_suspended: false,
            capturing: false,
            prev_foreground: None,
        };
        info!("进入事件循环（热键 {} 截图，托盘菜单退出）", app.config.hotkey);
        event_loop.run_app(&mut app).context("事件循环异常退出")?;
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn main() -> anyhow::Result<()> {
    imp::run()
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("PrismaSnap 仅支持 Windows（x86_64-pc-windows-msvc）");
}
