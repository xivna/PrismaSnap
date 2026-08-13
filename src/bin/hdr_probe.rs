//! HDR 捕获验证 spike（Phase 1 验收子项）。
//!
//! 目的（对应 AGENTS.md 3.2 节）：
//! 1. 验证 `windows-capture` 的 `ColorFormat::Rgba16F` 在 HDR 显示器下
//!    返回的到底是不是真正的 scRGB 线性数据。
//! 2. 验证 f16 → f32 → SDR 白点归一化 → knee/shoulder tone map → sRGB
//!    的完整转换链路（方案见 docs/关于HDR色彩转换技术方案与问题的回复.md）。
//!
//! 转换要点：
//! - scRGB 中 1.0 对应 80 nit；Windows HDR 模式会把 SDR 桌面内容提升到
//!   >1.0，必须先按 `SdrWhiteLevelInNits / 80` 归一化。
//! - tone map 采用 knee/shoulder 曲线（knee 以下恒等，之上平滑压缩），
//!   且只对亮度 Y 做映射以保色相；headroom 由显示器 MaxLuminance / SDR 白点
//!   计算（物理准确）。
//!
//! 运行：Windows 实机（建议开启 HDR，屏幕上有高亮内容），双击运行后捕获
//! 主显示器一帧，打印统计并在 exe 同目录生成 `hdr_probe_result.txt`
//! （UTF-8 报告）与 `hdr_probe_out.png`。
//!
//! 色彩转换已定稿（2026-08-13 实机对照实验）：线性增益 0.617 + 硬裁剪，
//! 与 Windows 自带 HDR 截图行为一致（SDR 白点 → sRGB 206，>435 nit 裁白）。
//! 被否决方案：knee/shoulder ease-out 滚降压扁中高调对比度（画面发灰）。

#[cfg(target_os = "windows")]
mod imp {
    use half::f16;
    use image::RgbaImage;
    use prismsnap::capture::color::{self, DEFAULT_GAIN};
    use prismsnap::capture::display_info;
    use std::path::PathBuf;

    use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
    use windows_capture::frame::{Frame, FrameBuffer};
    use windows_capture::graphics_capture_api::InternalCaptureControl;
    use windows_capture::monitor::Monitor;
    use windows_capture::settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    };

    /// HDR 转换参数（`run()` 查询后写入，`on_frame_arrived` 读取）。
    struct HdrParams {
        sdr_white_scrgb: f32,
        note: String,
    }

    static PARAMS: std::sync::OnceLock<HdrParams> = std::sync::OnceLock::new();

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
            println!(
                "Captured one frame: {}x{}, format {:?}",
                width,
                height,
                frame.color_format()
            );

            let params = match PARAMS.get() {
                Some(p) => p,
                None => {
                    return Err(anyhow::anyhow!("HDR params not initialized before capture").into())
                }
            };

            let mut buffer = frame.buffer()?;
            analyze_and_save(&mut buffer, width, height, params)?;

            capture_control.stop();
            Ok(())
        }

        fn on_closed(&mut self) -> Result<(), Self::Error> {
            println!("Capture session ended");
            Ok(())
        }
    }

    /// 归一化亮度直方图桶数（覆盖 [0, 2.0]）。
    const HIST_BINS: usize = 2048;
    /// 每单位亮度对应的桶数（桶宽 ≈ 0.00098）。
    const HIST_SCALE: f32 = 1024.0;

    /// 分析一帧 Rgba16F 数据，输出统计报告 + 定稿方案转换的 sRGB PNG。
    fn analyze_and_save(
        buffer: &mut FrameBuffer,
        width: u32,
        height: u32,
        params: &HdrParams,
    ) -> anyhow::Result<()> {
        let fmt = buffer.color_format();
        if !matches!(fmt, ColorFormat::Rgba16F) {
            anyhow::bail!("expected Rgba16F, got {:?}", fmt);
        }

        let w = width as usize;
        let h = height as usize;
        let row_pitch = buffer.row_pitch() as usize;
        let sdr_white_scrgb = params.sdr_white_scrgb;

        // 原始 scRGB 统计量
        let mut total: u64 = 0;
        let mut raw_over_one: u64 = 0;
        let mut norm_over_one: u64 = 0;
        let mut max_r = f32::NEG_INFINITY;
        let mut max_g = f32::NEG_INFINITY;
        let mut max_b = f32::NEG_INFINITY;
        let mut min_r = f32::INFINITY;
        let mut min_g = f32::INFINITY;
        let mut min_b = f32::INFINITY;
        let mut lum_sum: f64 = 0.0;
        let mut hist = [0u64; HIST_BINS];

        // 第一遍：统计 + 归一化亮度直方图
        {
            let raw = buffer.as_raw_buffer();
            for y in 0..h {
                let row_start = y * row_pitch;
                for x in 0..w {
                    // Rgba16F：每像素 4 通道，每通道 2 字节（f16），共 8 字节
                    let px = row_start + x * 8;
                    let r = read_f16(&raw[px..px + 2]);
                    let g = read_f16(&raw[px + 2..px + 4]);
                    let b = read_f16(&raw[px + 4..px + 6]);

                    total += 1;
                    if r > 1.0 || g > 1.0 || b > 1.0 {
                        raw_over_one += 1;
                    }
                    let rn = r / sdr_white_scrgb;
                    let gn = g / sdr_white_scrgb;
                    let bn = b / sdr_white_scrgb;
                    if rn > 1.0 || gn > 1.0 || bn > 1.0 {
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

                    let ylum = 0.2126 * rn + 0.7152 * gn + 0.0722 * bn;
                    let bin = ((ylum * HIST_SCALE).clamp(0.0, (HIST_BINS - 1) as f32)) as usize;
                    hist[bin] += 1;
                }
            }
        }

        // 黑位参考：最暗 0.1% 像素的归一化亮度（保留作诊断；此前已排除黑位抬升）
        let black_ref = percentile(&hist, total, 0.001);

        // 第二遍：按定稿方案（增益 + 硬裁剪）生成输出图
        let mut img = RgbaImage::new(width, height);
        {
            let raw = buffer.as_raw_buffer();
            for y in 0..h {
                let row_start = y * row_pitch;
                for x in 0..w {
                    let px = row_start + x * 8;
                    let r = read_f16(&raw[px..px + 2]);
                    let g = read_f16(&raw[px + 2..px + 4]);
                    let b = read_f16(&raw[px + 4..px + 6]);
                    let a = read_f16(&raw[px + 6..px + 8]);
                    let alpha = (a.clamp(0.0, 1.0) * 255.0).round() as u8;

                    let [sr, sg, sb] = color::hdr_to_srgb(r, g, b, sdr_white_scrgb);
                    img.put_pixel(x as u32, y as u32, image::Rgba([sr, sg, sb, alpha]));
                }
            }
        }

        let out_path = exe_dir()?.join("hdr_probe_out.png");
        img.save(&out_path)?;

        // 汇总报告（英文避免 Windows 控制台代码页乱码，result.txt 为 UTF-8）
        let mut report = String::new();
        report.push_str(&format!("Resolution: {}x{} ({} pixels)\n", w, h, total));
        report.push_str(&format!("Color format: {:?}\n", fmt));
        report.push_str(&format!("{}\n", params.note));
        report.push_str(&format!(
            "Tone map: linear gain {:.3} + hard clip (preserve hue, Windows-matched)\n",
            DEFAULT_GAIN
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
        report.push_str("\nNormalized luminance histogram (dark -> bright):\n");
        for (lo, hi, label) in [
            (0.0, 0.01, "  [0.00, 0.01)"),
            (0.01, 0.05, "  [0.01, 0.05)"),
            (0.05, 0.10, "  [0.05, 0.10)"),
            (0.10, 0.50, "  [0.10, 0.50)"),
            (0.50, 1.00, "  [0.50, 1.00)"),
            (1.00, 2.00, "  [1.00, 2.00)"),
        ] {
            let c = hist_range(&hist, lo, hi);
            report.push_str(&format!(
                "{}: {} ({:.4}%)\n",
                label,
                c,
                c as f64 / total as f64 * 100.0
            ));
        }
        let over_two = hist_range(&hist, 2.0, f32::INFINITY);
        report.push_str(&format!(
            "  [2.00, inf): {} ({:.4}%)\n",
            over_two,
            over_two as f64 / total as f64 * 100.0
        ));
        report.push_str(&format!(
            "Black point ref (0.1% percentile, normalized): {:.4}\n",
            black_ref
        ));
        report.push_str(&format!(
            "Output PNG (finalized gain pipeline): {}\n",
            out_path.display()
        ));

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

    /// 直方图第 `p` 分位的归一化亮度（`p ∈ [0,1]`，如 0.001 = 最暗 0.1%）。
    fn percentile(hist: &[u64; HIST_BINS], total: u64, p: f64) -> f32 {
        let target = ((total as f64) * p).round() as u64;
        let mut acc = 0u64;
        for (i, &c) in hist.iter().enumerate() {
            acc += c;
            if acc >= target.max(1) {
                return (i as f32 + 0.5) / HIST_SCALE;
            }
        }
        0.0
    }

    /// 直方图在 `[lo, hi)` 区间内的像素数。
    fn hist_range(hist: &[u64; HIST_BINS], lo: f32, hi: f32) -> u64 {
        let start = (lo * HIST_SCALE).max(0.0) as usize;
        let end = ((hi * HIST_SCALE).ceil() as usize).min(HIST_BINS);
        hist[start..end].iter().sum()
    }

    /// 从 2 字节小端数据读出 f16 并转 f32。
    fn read_f16(bytes: &[u8]) -> f32 {
        f16::from_bits(u16::from_le_bytes([bytes[0], bytes[1]])).to_f32()
    }

    /// 可执行文件所在目录（便携式路径基准）。
    fn exe_dir() -> anyhow::Result<PathBuf> {
        let exe = std::env::current_exe()?;
        Ok(exe
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".")))
    }

    /// 查询目标显示器的 HDR 参数（SDR 白点用于转换，最大亮度仅作诊断展示）。
    fn query_hdr_params(device_name: &str) -> HdrParams {
        let sdr_white_nits = match display_info::query_sdr_white_nits(device_name) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("  SDR white query failed: {e}, fallback 80 nit");
                80.0
            }
        };
        let sdr_white_scrgb = sdr_white_nits / 80.0;

        // 最大亮度不再参与色调映射（增益 + 硬裁剪不需要 headroom），仅记录供诊断
        let max_luminance_nits = match display_info::query_max_luminance_nits(device_name) {
            Ok(m) => Some(m),
            Err(e) => {
                eprintln!("  Max luminance query failed: {e} (diagnostic only, ignored)");
                None
            }
        };

        let max_lum_desc = max_luminance_nits
            .map(|m| format!("{m} nit"))
            .unwrap_or_else(|| "unknown".to_string());
        let note = format!(
            "SDR white {} nit (scRGB {:.4}), max lum {}, gain {:.3}",
            sdr_white_nits, sdr_white_scrgb, max_lum_desc, DEFAULT_GAIN
        );

        HdrParams {
            sdr_white_scrgb,
            note,
        }
    }

    pub fn run() -> anyhow::Result<()> {
        println!("=== PrismaSnap HDR capture probe ===\n");

        // 先确定目标显示器（spike 验证主屏），并查询其 HDR 参数
        let primary = Monitor::primary()?;
        let device_name = primary
            .device_name()
            .unwrap_or_else(|_| "\\\\.\\DISPLAY1".to_string());
        println!(
            "Primary monitor: {} ({}x{}, device {})\n",
            primary.name().unwrap_or_else(|_| "?".to_string()),
            primary.width().unwrap_or(0),
            primary.height().unwrap_or(0),
            device_name
        );

        println!("Querying HDR params...");
        let params = query_hdr_params(&device_name);
        println!("  {}", params.note);
        let _ = PARAMS.set(params);

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

        println!("\nCapturing primary monitor...\n");

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

        println!("\nDone. See hdr_probe_result.txt / hdr_cmp_*.png next to this exe.");
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
