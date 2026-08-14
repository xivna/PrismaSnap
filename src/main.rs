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
//! 发布版无控制台窗口（GUI 程序），日志写入 exe 同目录 logs/；
//! 调试需要控制台时用 `--features console` 编译。

// 无控制台窗口（除非启用 console feature 调试）
#![cfg_attr(
    all(target_os = "windows", not(feature = "console")),
    windows_subsystem = "windows"
)]

#[cfg(target_os = "windows")]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
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
    use prismsnap::ui::overlay::{CapturedShot, Overlay};
    use prismsnap::ui::tray::{Tray, TrayAction};
    use prismsnap::utils::{logging, paths, single_instance};

    /// winit 自定义事件：捕获线程完成截图后经 `EventLoopProxy` 唤醒主循环。
    enum UserEvent {
        CaptureDone(Result<CapturedShot, String>),
    }

    /// 应用状态：托盘 + 热键 + 覆盖层窗口。
    struct App {
        config: Arc<Config>,
        tray: Tray,
        /// 消息钩子检测到热键时置位，主循环在 `about_to_wait` 里消费。
        hotkey_triggered: Arc<AtomicBool>,
        /// 捕获线程回传结果的通道（克隆进线程）。
        proxy: EventLoopProxy<UserEvent>,
        /// 当前覆盖层窗口（None 表示无截图会话）。
        overlay: Option<Overlay>,
        /// 捕获进行中（防止热键连按重复触发）。
        capturing: bool,
    }

    impl App {
        /// 触发一次截图：起独立线程执行捕获链路，完成经 proxy 回传。
        fn trigger_capture(&mut self) {
            if self.capturing || self.overlay.is_some() {
                return;
            }
            self.capturing = true;
            info!("触发截图");
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
            // 隐藏状态下同步渲染首帧（GPU 初始化已完成），再显示窗口，
            // 避免露出未渲染的默认背景造成黑白闪烁
            overlay.redraw();
            window.set_visible(true);
            // 覆盖层需要键盘输入（Esc/Enter/Ctrl+C/Ctrl+S），显示后取焦点
            window.focus_window();
            window.request_redraw();
            self.overlay = Some(overlay);
        }

        /// 处理覆盖层退出请求（销毁窗口，回到等待热键状态）。
        fn close_overlay(&mut self, event_loop: &ActiveEventLoop) {
            info!("关闭覆盖层");
            self.overlay = None;
            event_loop.set_control_flow(ControlFlow::Wait);
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
            let Some(overlay) = self.overlay.as_mut() else {
                return;
            };
            if overlay.window_id() != window_id {
                return;
            }
            overlay.on_window_event(&event);
            if overlay.exit_requested {
                self.close_overlay(event_loop);
            }
        }

        fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
            match event {
                UserEvent::CaptureDone(Ok(shot)) => {
                    self.capturing = false;
                    info!("截图完成: {}x{}", shot.img.width(), shot.img.height());
                    self.open_overlay(event_loop, shot);
                    // 覆盖层期间持续轮询（egui 重绘 / 光标移动）
                    event_loop.set_control_flow(ControlFlow::Poll);
                }
                UserEvent::CaptureDone(Err(e)) => {
                    self.capturing = false;
                    error!("截图失败: {e}");
                }
            }
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            if self.hotkey_triggered.swap(false, Ordering::SeqCst) {
                self.trigger_capture();
            }
            match self.tray.poll_action() {
                Some(TrayAction::Capture) => self.trigger_capture(),
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
        let raw = engine::capture_frame(monitor).map_err(|e| e.to_string())?;
        info!("捕获完成: {}x{}", raw.width, raw.height);

        // HDR 降级开关：直接把 scRGB clamp 当 SDR 处理（SDR 白点 = 1.0）
        let sdr_white = if config.capture.hdr_degrade {
            1.0
        } else {
            match display_info::query_sdr_white_nits(&raw.device_name) {
                Ok(n) => {
                    info!("SDR 白点: {n} nit");
                    n / 80.0
                }
                Err(e) => {
                    warn!("SDR 白点查询失败: {e}，回退 80 nit");
                    1.0
                }
            }
        };
        let img = frame::frame_to_srgb_image(&raw, sdr_white);
        Ok(CapturedShot { img, monitor_rect })
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

        // 2. 日志（文件句柄须持有到进程退出）
        let _log_file = logging::init(&paths::exe_dir()?.join("logs"))?;
        info!("PrismaSnap 启动");

        // panic 兜底：把崩溃栈写进日志（发布版无控制台，不设 hook 什么都看不到）
        std::panic::set_hook(Box::new(|info| {
            error!("panic: {info}");
            eprintln!("panic: {info}");
        }));

        // 3. 配置
        let config_path = Config::default_path()?;
        let config = match Config::load(&config_path) {
            Ok(c) => c,
            Err(e) => {
                warn!("配置文件损坏，使用默认配置: {e}");
                Config::default()
            }
        };
        info!("配置加载: {}", config_path.display());

        // 4. 全局热键（在事件循环创建前注册，id 供消息钩子比对）
        let hotkeys = HotkeyManager::register(&config.hotkey).with_context(|| {
            format!("注册全局热键失败（可能被其他程序占用）: {}", config.hotkey)
        })?;
        let hotkey_id = hotkeys.id();
        info!("全局热键已注册: {} (id {hotkey_id})", config.hotkey);

        // 5. 托盘
        let tray = Tray::new()?;

        // 6. winit 事件循环：WM_HOTKEY 检测走消息钩子（线程消息队列级）
        let hotkey_triggered = Arc::new(AtomicBool::new(false));
        let mut builder = EventLoop::<UserEvent>::with_user_event();
        {
            let flag = hotkey_triggered.clone();
            builder.with_msg_hook(move |msg| {
                use windows::Win32::UI::WindowsAndMessaging::{MSG, WM_HOTKEY};
                // Safety: winit 保证传入的指针指向有效的 MSG
                let msg = unsafe { &*(msg as *const MSG) };
                if msg.message == WM_HOTKEY && msg.wParam.0 as u32 == hotkey_id {
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
            tray,
            hotkey_triggered,
            proxy,
            overlay: None,
            capturing: false,
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
