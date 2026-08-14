//! PrismaSnap 程序入口。
//!
//! 最终形态：启动后默认无主窗口，仅显示系统托盘图标，全局快捷键触发截图。
//!
//! 当前为 Phase 2 核心链路 spike：运行即执行
//! 「定位鼠标所在显示器 → 捕获一帧 Rgba16F → HDR 色彩转换 → 保存 PNG → 复制剪贴板」，
//! 验证捕获引擎 / 转换链路 / 输出封装三大件后再搭框架外壳（托盘/热键）。

#[cfg(target_os = "windows")]
mod imp {
    use prismsnap::capture::{display_info, engine, frame};
    use prismsnap::utils::{clipboard, image_codec, paths};

    /// 查询 SDR 白点（nit → scRGB），失败时回退标准 SDR 白点 80 nit。
    fn query_sdr_white_scrgb(device_name: &str) -> f32 {
        match display_info::query_sdr_white_nits(device_name) {
            Ok(n) => {
                println!("SDR 白点: {n} nit");
                n / 80.0
            }
            Err(e) => {
                eprintln!("SDR 白点查询失败: {e}，回退 80 nit");
                1.0
            }
        }
    }

    /// spike 主流程。
    pub fn run() -> anyhow::Result<()> {
        println!("=== PrismaSnap 核心链路 spike ===");

        // 1. 定位鼠标光标所在显示器
        let monitor = engine::monitor_at_cursor()?;
        let device_name = monitor
            .device_name()
            .unwrap_or_else(|_| String::from("\\\\.\\DISPLAY1"));
        println!(
            "目标显示器: {} ({}x{}, {})",
            monitor.name().unwrap_or_else(|_| String::from("?")),
            monitor.width().unwrap_or(0),
            monitor.height().unwrap_or(0),
            device_name
        );

        // 2. 捕获一帧（独立线程 + channel，见 capture/engine.rs）
        println!("捕获中...");
        let raw = engine::capture_frame(monitor)?;
        println!("捕获完成: {}x{}", raw.width, raw.height);

        // 3. HDR 色彩转换（定稿方案：SDR 白点归一化 + 线性增益 + 硬裁剪）
        let sdr_white_scrgb = query_sdr_white_scrgb(&raw.device_name);
        let img = frame::frame_to_srgb_image(&raw, sdr_white_scrgb);

        // 4. 保存 PNG 到 exe 同目录（便携式路径）
        let out_path = paths::exe_dir()?.join("prismsnap_capture.png");
        image_codec::save_png(&img, &out_path)?;
        println!("已保存: {}", out_path.display());

        // 5. 复制到剪贴板
        clipboard::copy_image(&img)?;
        println!("已复制到剪贴板（可在画图/聊天软件 Ctrl+V 验证）");

        println!("\n按回车退出...");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
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
