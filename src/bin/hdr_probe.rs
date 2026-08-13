//! HDR 捕获验证 spike（Phase 1 验收子项）。
//!
//! 目的（对应 AGENTS.md 3.2 节）：
//! 1. 验证 `windows-capture` 的 `ColorFormat::Rgba16F` 在 HDR 显示器下
//!    返回的到底是不是真正的 scRGB 线性数据。
//! 2. 验证 f16 → f32 → SDR 白点归一化 → tone map → sRGB 的完整转换链路。
//!
//! 转换要点（3.2 节"SDR 白点问题"的实证修正）：
//! - scRGB 中 1.0 对应 80 nit；Windows HDR 模式会把 SDR 桌面内容提升到
//!   >1.0，必须先按 `SdrWhiteLevelInNits / 80` 归一化。
//! - 归一化后 SDR 白点 = 1.0；>1.0 的是真 HDR 高光。直接裁剪会让高光
//!   变成一坨纯白（无细节），需用 rolloff 软滚降保留层次。
//!
//! 运行：Windows 实机（建议开启 HDR，屏幕上有高亮内容），双击运行后捕获
//! 主显示器一帧，打印统计并在 exe 同目录生成 `hdr_probe_out.png` 与
//! `hdr_probe_result.txt`（UTF-8 报告）。

#[cfg(target_os = "windows")]
mod imp {
    use half::f16;
    use image::RgbaImage;
    use std::path::PathBuf;

    use windows::Graphics::Display::DisplayInformation;
    use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
    use windows_capture::frame::{Frame, FrameBuffer};
    use windows_capture::graphics_capture_api::InternalCaptureControl;
    use windows_capture::monitor::Monitor;
    use windows_capture::settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    };

    /// SDR 白点归一化因子（scRGB），由 run() 查询后写入。
    static SDR_WHITE_SCRGB: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    /// SDR 白点来源说明（是否查询成功 / fallback）。
    static SDR_WHITE_NOTE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

    /// SDR 白点在输出中的位置（<1.0 为 HDR 高光留 headroom）。
    /// 8-bit SDR 输出的物理限制：SDR 白点映射到 1.0 时高光无空间（纯白）；
    /// 值越小高光层次越多、但 SDR 白色越发灰。0.7 ≈ 留 30% headroom（约 68 级灰阶）。
    const SDR_OUT_WHITE: f32 = 0.7;

    /// 捕获句柄：在 `on_frame_arrived` 中处理一帧后立即停止。
    struct CaptureHandler;

    impl GraphicsCaptureApiHandler for CaptureHandler {
        type Flags = ();
        type Error = Box<dyn std::error::Error + Send + Sync>;

        fn new(_ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
            Ok(Self)
        }

        fn on_frame_arrived(
            &mut self,
            frame: &mut Frame,
            capture_control: InternalCaptureControl,
        ) -> Result<(), Self::Error> {
            let width = frame.width();
            let height = frame.height();
            let sdr_white_scrgb = SDR_WHITE_SCRGB.get().copied().unwrap_or(1.0);
            println!(
                "Captured one frame: {}x{}, format {:?}",
                width,
                height,
                frame.color_format()
            );

            let mut buffer = frame.buffer()?;
            analyze_and_save(&mut buffer, width, height, sdr_white_scrgb)?;

            capture_control.stop();
            Ok(())
        }

        fn on_closed(&mut self) -> Result<(), Self::Error> {
            println!("Capture session ended");
            Ok(())
        }
    }

    /// 分析一帧 Rgba16F 数据并保存 sRGB PNG。
    fn analyze_and_save(
        buffer: &mut FrameBuffer,
        width: u32,
        height: u32,
        sdr_white_scrgb: f32,
    ) -> anyhow::Result<()> {
        let fmt = buffer.color_format();
        if !matches!(fmt, ColorFormat::Rgba16F) {
            anyhow::bail!("expected Rgba16F, got {:?}", fmt);
        }

        let w = width as usize;
        let h = height as usize;
        let row_pitch = buffer.row_pitch() as usize;
        let raw = buffer.as_raw_buffer();

        // 原始 scRGB 统计量
        let mut total: u64 = 0;
        let mut raw_over_one: u64 = 0;
        let mut max_r = f32::NEG_INFINITY;
        let mut max_g = f32::NEG_INFINITY;
        let mut max_b = f32::NEG_INFINITY;
        let mut min_r = f32::INFINITY;
        let mut min_g = f32::INFINITY;
        let mut min_b = f32::INFINITY;
        let mut lum_sum: f64 = 0.0;

        // 归一化后 >1.0 的像素（真正的 HDR 高光）
        let mut norm_over_one: u64 = 0;

        // 输出图（归一化 + tone map + sRGB 后）
        let mut img = RgbaImage::new(width, height);

        for y in 0..h {
            let row_start = y * row_pitch;
            for x in 0..w {
                // Rgba16F：每像素 4 通道，每通道 2 字节（f16），共 8 字节
                let px = row_start + x * 8;
                let r = read_f16(&raw[px..px + 2]);
                let g = read_f16(&raw[px + 2..px + 4]);
                let b = read_f16(&raw[px + 4..px + 6]);
                let a = read_f16(&raw[px + 6..px + 8]);

                total += 1;
                if r > 1.0 || g > 1.0 || b > 1.0 {
                    raw_over_one += 1;
                }
                if r / sdr_white_scrgb > 1.0
                    || g / sdr_white_scrgb > 1.0
                    || b / sdr_white_scrgb > 1.0
                {
                    norm_over_one += 1;
                }
                max_r = max_r.max(r);
                max_g = max_g.max(g);
                max_b = max_b.max(b);
                min_r = min_r.min(r);
                min_g = min_g.min(g);
                min_b = min_b.min(b);
                // 亮度粗略按 Rec.709 加权
                lum_sum += 0.2126 * r as f64 + 0.7152 * g as f64 + 0.0722 * b as f64;

                img.put_pixel(
                    x as u32,
                    y as u32,
                    image::Rgba([
                        linear_to_srgb(r, sdr_white_scrgb),
                        linear_to_srgb(g, sdr_white_scrgb),
                        linear_to_srgb(b, sdr_white_scrgb),
                        (a.clamp(0.0, 1.0) * 255.0).round() as u8,
                    ]),
                );
            }
        }

        let out_path = exe_dir()?.join("hdr_probe_out.png");
        img.save(&out_path)?;

        // 汇总报告（英文避免 Windows 控制台代码页乱码，result.txt 为 UTF-8）
        let note = SDR_WHITE_NOTE
            .get()
            .map(|s| s.as_str())
            .unwrap_or("unknown");
        let mut report = String::new();
        report.push_str(&format!("Resolution: {}x{} ({} pixels)\n", w, h, total));
        report.push_str(&format!("Color format: {:?}\n", fmt));
        report.push_str(&format!("SDR white: {} (scRGB factor {:.4})\n", note, sdr_white_scrgb));
        report.push_str(&format!(
            "Tone map: SDR linear to {:.2}, highlights log rolloff to 1.0\n",
            SDR_OUT_WHITE
        ));
        report.push_str(&format!("R channel: min={:.4} max={:.4}\n", min_r, max_r));
        report.push_str(&format!("G channel: min={:.4} max={:.4}\n", min_g, max_g));
        report.push_str(&format!("B channel: min={:.4} max={:.4}\n", min_b, max_b));
        report.push_str(&format!(
            "Average luminance (linear, raw scRGB): {:.4}\n",
            lum_sum / total as f64
        ));
        report.push_str(&format!(
            "Pixels over 1.0 (raw scRGB): {} ({:.4}%)\n",
            raw_over_one,
            raw_over_one as f64 / total as f64 * 100.0
        ));
        report.push_str(&format!(
            "Pixels over 1.0 (normalized, true HDR highlights): {} ({:.4}%)\n",
            norm_over_one,
            norm_over_one as f64 / total as f64 * 100.0
        ));
        report.push_str(&format!("Output PNG: {}\n", out_path.display()));

        if raw_over_one > 0 {
            report.push_str(">>> RESULT: HDR (scRGB) data detected (raw pixels > 1.0).\n");
        } else {
            report.push_str(
                ">>> RESULT: all raw pixels <= 1.0. Display may be in SDR mode, or WGC tone-mapped.\n",
            );
        }

        println!("{}", report);
        std::fs::write(exe_dir()?.join("hdr_probe_result.txt"), &report)?;

        Ok(())
    }

    /// 从 2 字节小端数据读出 f16 并转 f32。
    fn read_f16(bytes: &[u8]) -> f32 {
        f16::from_bits(u16::from_le_bytes([bytes[0], bytes[1]])).to_f32()
    }

    /// HDR → SDR 色调映射：SDR 白点线性 + 高光对数滚降。
    ///
    /// `x` 为 scRGB / sdr_white（SDR 白点 = 1.0）：
    /// - x ≤ 1（SDR 内容）线性映射到 [0, sdr_out_white]，保证 SDR 所见即所得；
    /// - x > 1（HDR 高光）对数滚降到 (sdr_out_white, 1.0]，保留高光层次而非硬裁剪。
    fn tone_map(x: f32, sdr_out_white: f32) -> f32 {
        if x <= 1.0 {
            x * sdr_out_white
        } else {
            // 对数滚降：x = 1 + max_extra 时到满白；max_extra 覆盖常见高光范围
            let max_extra = 6.0f32;
            let t = (x - 1.0).ln_1p() / max_extra.ln_1p();
            sdr_out_white + (1.0 - sdr_out_white) * t.min(1.0)
        }
    }

    /// 归一化 + tone map + linear→sRGB gamma。
    fn linear_to_srgb(v: f32, sdr_white_scrgb: f32) -> u8 {
        let x = tone_map(v / sdr_white_scrgb, SDR_OUT_WHITE).clamp(0.0, 1.0);
        let s = if x <= 0.0031308 {
            x * 12.92
        } else {
            1.055 * x.powf(1.0 / 2.4) - 0.055
        };
        (s * 255.0).round().clamp(0.0, 255.0) as u8
    }

    /// 可执行文件所在目录（便携式路径基准）。
    fn exe_dir() -> anyhow::Result<PathBuf> {
        let exe = std::env::current_exe()?;
        Ok(exe
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".")))
    }

    /// 查询 Windows SDR 白点（nit），换算成 scRGB 归一化因子。
    fn query_sdr_white() -> (f32, String) {
        // 确保 WinRT 已初始化（windows-capture 内部也会 RoInitialize，重复调用安全）
        unsafe {
            let _ = windows::Win32::System::WinRT::RoInitialize(
                windows::Win32::System::WinRT::RO_INIT_MULTITHREADED,
            );
        }

        match DisplayInformation::GetForCurrentView()
            .and_then(|info| info.GetAdvancedColorInfo())
            .and_then(|adv| adv.SdrWhiteLevelInNits())
        {
            Ok(nits) => {
                // scRGB 中 1.0 = 80 nit
                (nits / 80.0, format!("{} nit", nits))
            }
            Err(e) => {
                // 控制台程序可能无 current view，回退到常见 HDR 模式 SDR 亮度（scRGB 2.0）
                (2.0, format!("query failed ({}), fallback 2.0", e))
            }
        }
    }

    pub fn run() -> anyhow::Result<()> {
        println!("=== PrismaSnap HDR capture probe ===\n");

        // 查询 SDR 白点（在捕获前，避免与捕获线程竞争 COM）
        let (sdr_white_scrgb, note) = query_sdr_white();
        let _ = SDR_WHITE_SCRGB.set(sdr_white_scrgb);
        let _ = SDR_WHITE_NOTE.set(note.clone());
        println!("SDR white level: {}", note);

        // 枚举所有显示器
        let monitors = Monitor::enumerate()?;
        println!("\nFound {} monitor(s):", monitors.len());
        for (i, m) in monitors.iter().enumerate() {
            println!(
                "  #{} {} : {}x{} @{}Hz",
                i + 1,
                m.name().unwrap_or_else(|_| "?".to_string()),
                m.width().unwrap_or(0),
                m.height().unwrap_or(0),
                m.refresh_rate().unwrap_or(0)
            );
        }

        // 捕获主显示器（spike 阶段先验证主屏；正式实现再按鼠标所在屏）
        let primary = Monitor::primary()?;
        println!(
            "\nCapturing primary monitor: {} ({}x{})...\n",
            primary.name().unwrap_or_else(|_| "?".to_string()),
            primary.width().unwrap_or(0),
            primary.height().unwrap_or(0)
        );

        let settings = Settings::new(
            primary,
            CursorCaptureSettings::WithoutCursor,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Rgba16F,
            (),
        );

        CaptureHandler::start(settings)?;

        println!("\nDone. See hdr_probe_result.txt / hdr_probe_out.png next to this exe.");
        println!("Press Enter to exit...");
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
    eprintln!("hdr_probe only runs on Windows");
}
