//! PrismaSnap 程序入口。
//!
//! 启动流程：单实例检测 → 日志 → 配置 → 托盘 → 全局热键 → 常驻消息循环。
//! 无主窗口（设置界面 Phase 2 后续接入）；热键或托盘菜单触发截图：
//! 「定位鼠标所在屏 → Rgba16F 捕获 → HDR 转换 → 按配置保存 → 复制剪贴板」。

#[cfg(target_os = "windows")]
mod imp {
    use std::path::PathBuf;
    use std::thread::sleep;
    use std::time::Duration;

    use tracing::{error, info, warn};

    use prismsnap::capture::{display_info, engine, frame};
    use prismsnap::config::{Config, SaveFormat, SaveMode};
    use prismsnap::hotkey::{self, HotkeyManager};
    use prismsnap::ui::tray::{Tray, TrayAction};
    use prismsnap::utils::{clipboard, image_codec, logging, paths, single_instance, time};

    /// 消息循环轮询间隔。
    const LOOP_INTERVAL: Duration = Duration::from_millis(20);

    /// 查询目标显示器 SDR 白点（scRGB 单位），失败回退标准 80 nit。
    fn query_sdr_white_scrgb(device_name: &str) -> f32 {
        match display_info::query_sdr_white_nits(device_name) {
            Ok(n) => {
                info!("SDR 白点: {n} nit");
                n / 80.0
            }
            Err(e) => {
                warn!("SDR 白点查询失败: {e}，回退 80 nit");
                1.0
            }
        }
    }

    /// 解析保存目录：配置为空时用 exe 目录下 `screenshots/`。
    fn resolve_save_dir(dir: &PathBuf) -> PathBuf {
        if dir.as_os_str().is_empty() {
            paths::exe_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("screenshots")
        } else if dir.is_relative() {
            paths::exe_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(dir)
        } else {
            dir.clone()
        }
    }

    /// 按配置把截图保存到文件，返回保存路径。
    fn save_shot(img: &image::RgbaImage, config: &Config) -> anyhow::Result<PathBuf> {
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

    /// 截图主链路：定位 → 捕获 → HDR 转换 → 保存 → 剪贴板。
    fn capture_and_output(config: &Config) -> anyhow::Result<()> {
        info!("触发截图");
        let monitor = engine::monitor_at_cursor()?;
        let raw = engine::capture_frame(monitor)?;
        info!("捕获完成: {}x{}", raw.width, raw.height);

        // HDR 降级开关：直接把 scRGB clamp 当 SDR 处理（SDR 白点 = 1.0）
        let sdr_white = if config.capture.hdr_degrade {
            1.0
        } else {
            query_sdr_white_scrgb(&raw.device_name)
        };
        let img = frame::frame_to_srgb_image(&raw, sdr_white);

        match config.save.mode {
            SaveMode::Silent => {
                let path = save_shot(&img, config)?;
                info!("已静默保存: {}", path.display());
            }
            // Phase 2 无对话框：暂时同样静默保存（待设置 UI 上线后弹原生保存对话框）
            SaveMode::AlwaysAsk => {
                let path = save_shot(&img, config)?;
                info!("AlwaysAsk 暂退化为静默保存: {}", path.display());
            }
        }

        clipboard::copy_image(&img)?;
        info!("已复制到剪贴板");
        Ok(())
    }

    /// 泵取 Windows 窗口消息（托盘图标依赖消息泵才能收到菜单事件）。
    fn pump_messages() {
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
        };
        let mut msg = MSG::default();
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    pub fn run() -> anyhow::Result<()> {
        // 1. 单实例检测
        let _instance = match single_instance::acquire("Global\\PrismaSnap-SingleInstance")? {
            Some(guard) => guard,
            None => {
                eprintln!("PrismaSnap 已在运行（托盘区查看），本次启动退出。");
                return Ok(());
            }
        };

        // 2. 日志（文件句柄须持有到进程退出）
        let _log_file = logging::init(&paths::exe_dir()?.join("logs"))?;
        info!("PrismaSnap 启动");

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

        // 4. 托盘
        let tray = Tray::new()?;

        // 5. 全局热键
        let hotkeys = match HotkeyManager::register(&config.hotkey) {
            Ok(h) => {
                info!("全局热键已注册: {}", config.hotkey);
                h
            }
            Err(e) => {
                warn!("全局热键注册失败: {e}");
                return Err(e);
            }
        };

        info!("进入常驻循环（热键 {} 截图，托盘菜单退出）", config.hotkey);

        // 6. 消息循环
        loop {
            pump_messages();

            if hotkey::poll_trigger(hotkeys.id()) {
                if let Err(e) = capture_and_output(&config) {
                    error!("截图失败: {e:#}");
                }
            }

            match tray.poll_action() {
                Some(TrayAction::Capture) => {
                    if let Err(e) = capture_and_output(&config) {
                        error!("截图失败: {e:#}");
                    }
                }
                Some(TrayAction::Exit) => {
                    info!("托盘菜单退出");
                    break;
                }
                None => {}
            }

            sleep(LOOP_INTERVAL);
        }
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
