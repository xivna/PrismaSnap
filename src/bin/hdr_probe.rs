//! HDR 捕获验证 spike（Phase 1 验收子项，已重构复用正式捕获引擎）。
//!
//! 目的（对应 AGENTS.md 3.2 节）：
//! 1. 验证 `windows-capture` 的 `ColorFormat::Rgba16F` 在 HDR 显示器下
//!    返回的到底是不是真正的 scRGB 线性数据。
//! 2. 验证 f16 → f32 → SDR 白点归一化 → sRGB 的完整转换链路。
//!
//! 捕获与转换复用 `prismsnap::capture::engine` / `frame`（与 `main` 同一链路），
//! 本程序额外输出亮度统计报告用于诊断。
//!
//! 运行：Windows 实机（建议开启 HDR，屏幕上有高亮内容），双击运行后捕获
//! 主显示器一帧，打印统计并在 exe 同目录生成 `hdr_probe_result.txt`
//! （UTF-8 报告）与 `hdr_probe_out.png`。
//!
//! 色彩转换已定稿（2026-08-13 实机对照实验）：线性增益 0.617 + 硬裁剪，
//! 与 Windows 自带 HDR 截图行为一致（SDR 白点 → sRGB 206，>435 nit 裁白）。

#[cfg(target_os = "windows")]
mod imp {
    use prismsnap::capture::frame::{read_f16, RawFrame};
    use prismsnap::capture::{color, display_info, engine};
    use prismsnap::utils::{image_codec, paths};

    /// HDR 转换参数（查询后写入报告，供统计口径使用）。
    struct HdrParams {
        sdr_white_scrgb: f32,
        note: String,
    }

    /// 归一化亮度直方图桶数（覆盖 [0, 2.0]）。
    const HIST_BINS: usize = 2048;
    /// 每单位亮度对应的桶数（桶宽 ≈ 0.00098）。
    const HIST_SCALE: f32 = 1024.0;

    /// 分析一帧 Rgba16F 数据，输出统计报告 + 定稿方案转换的 sRGB PNG。
    fn analyze_and_save(raw: &RawFrame, params: &HdrParams) -> anyhow::Result<()> {
        let w = raw.width as usize;
        let h = raw.height as usize;
        let row_pitch = raw.row_pitch;
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

        // 统计遍历：Rgba16F 每像素 8 字节，按 row_pitch 定位行
        for y in 0..h {
            let row_start = y * row_pitch;
            for x in 0..w {
                let px = row_start + x * 8;
                let r = read_f16(&raw.data[px..px + 2]);
                let g = read_f16(&raw.data[px + 2..px + 4]);
                let b = read_f16(&raw.data[px + 4..px + 6]);

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

        // 黑位参考：最暗 0.1% 像素的归一化亮度（保留作诊断；此前已排除黑位抬升）
        let black_ref = percentile(&hist, total, 0.001);

        // 转换（复用正式链路，与 main 一致）
        let img = prismsnap::capture::frame::frame_to_srgb_image(raw, sdr_white_scrgb);
        let out_path = paths::exe_dir()?.join("hdr_probe_out.png");
        image_codec::save_png(&img, &out_path)?;

        // 汇总报告（英文避免 Windows 控制台代码页乱码，result.txt 为 UTF-8）
        let mut report = String::new();
        report.push_str(&format!("Resolution: {}x{} ({} pixels)\n", w, h, total));
        report.push_str("Color format: Rgba16F\n");
        report.push_str(&format!("{}\n", params.note));
        report.push_str(&format!(
            "Tone map: linear gain {:.3} + hard clip (preserve hue, Windows-matched)\n",
            color::DEFAULT_GAIN
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
        std::fs::write(paths::exe_dir()?.join("hdr_probe_result.txt"), &report)?;

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
            sdr_white_nits,
            sdr_white_scrgb,
            max_lum_desc,
            color::DEFAULT_GAIN
        );

        HdrParams {
            sdr_white_scrgb,
            note,
        }
    }

    pub fn run() -> anyhow::Result<()> {
        use windows_capture::monitor::Monitor;

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

        // 捕获（复用正式引擎：独立线程 + channel）
        let raw = engine::capture_frame(primary)?;
        analyze_and_save(&raw, &params)?;

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
