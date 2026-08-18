//! HDR（scRGB）→ SDR（sRGB）色彩转换纯函数。
//!
//! 无任何 Windows / 平台依赖，跨平台可编译，配单元测试。
//!
//! 背景（对应 AGENTS.md 3.2 节与 docs/ 里的技术方案）：
//! - `windows-capture` 的 `Rgba16F` 返回的是 scRGB 线性 f16 数据，
//!   其中 1.0 = 80 nit（SDR 白点），高光部分数值可远超 1.0。
//! - 8-bit SDR 输出物理上只有 256 级，必须做 tone map 才能在
//!   "SDR 内容正确"与"HDR 高光有细节"之间取舍。
//!
//! 定稿方案（2026-08-13，依据实机对照实验，详见 PROGRESS.md 决策记录）：
//! - **线性增益 + 硬裁剪**：归一化后乘以常数增益（默认 0.617），
//!   超过 1.0 的高光直接裁白。线性域乘常数 = gamma 域整体平移，
//!   **局部对比度无损**——这是与 Windows 自带 HDR 截图（Xbox Game Bar）
//!   实测一致的行为（SDR 白点 268 nit → sRGB 206，>435 nit 裁白）。
//! - 被否决的旧方案：knee/shoulder ease-out 滚降在 knee 以上区间局部斜率
//!   仅 ~0.17，把占画面大头的 0.7~1.5 归一化亮度（雾、天空、浅色 UI）
//!   对比度压掉 5~6 倍，是"画面发灰"的根源；黑位抬升假设亦被实机排除。
//! - **保色相**：只对亮度 Y 做映射，把同一缩放系数应用回 RGB 三通道，
//!   避免三个通道各自压缩比例不一致导致的色相偏移。

/// Rec.709 亮度加权系数（scRGB/sRGB 均采用该组系数）。
pub const LUM_R: f32 = 0.2126;
pub const LUM_G: f32 = 0.7152;
pub const LUM_B: f32 = 0.0722;

/// 默认线性增益（HDR → SDR 转换）。
///
/// 实测来源（2026-08-13）：Windows 自带 HDR 截图把 SDR 白点（268 nit）
/// 映射到 sRGB 206，即线性增益 ≈ 0.617；>435 nit 的高光直接裁白。
/// 观感微调空间大致在 0.6~0.8 之间（越大 SDR 内容越亮、高光裁得越早），
/// 将来如需做成用户配置项改这里即可。
pub const DEFAULT_GAIN: f32 = 0.617;

/// 线性增益 + 短肩部映射。
///
/// 本函数在"增益后线性域"工作，入参 `y` 为已按 SDR 白点归一化的
/// 线性亮度（SDR 白点 = 1.0），应非负：
/// - `y * gain <= knee`：恒等输出（斜率 = gain，对比度无损）；
/// - `knee < y * gain <= max`：ease-out 压缩到 `[knee, 1]`（短肩部，
///   仅用于软化裁剪边界，跨度应远小于 knee 以下的恒等区间）；
/// - `y * gain > max`：裁剪为 1.0。
/// - `max <= knee` 时退化为纯增益 + 硬裁剪（Windows 截图同款，默认用法）。
pub fn gain_map(y: f32, gain: f32, knee: f32, max: f32) -> f32 {
    let yg = y * gain;
    if max <= knee {
        // 无肩部：纯增益 + 硬裁剪
        return yg.clamp(0.0, 1.0);
    }
    if yg <= knee {
        yg
    } else {
        let t = ((yg - knee) / (max - knee)).min(1.0);
        knee + (1.0 - knee) * (t * (2.0 - t))
    }
}

/// linear → sRGB gamma 编码（单通道，输入应为 `[0, 1]` 线性值）。
pub fn linear_to_srgb_gamma(x: f32) -> f32 {
    if x <= 0.003_130_8 {
        x * 12.92
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// 线性 SDR → sRGB 8-bit 直通转换（无增益、无归一化），输出 `[r, g, b]`。
///
/// 适用于系统未开启 HDR 的显示器：此时 WGC 的 `Rgba16F` 缓冲就是
/// 0~1.0 的线性 sRGB 数据（不存在 >1.0 高光），直接 gamma 编码即为原图。
/// 不应复用 [`hdr_to_srgb`]（其默认增益 0.617 会整体压暗画面）。
pub fn sdr_linear_to_srgb(r: f32, g: f32, b: f32) -> [u8; 3] {
    let to_byte = |v: f32| {
        (linear_to_srgb_gamma(v.clamp(0.0, 1.0)) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    [to_byte(r), to_byte(g), to_byte(b)]
}

/// 保色相 HDR → SDR 转换（默认参数），输出 sRGB 8-bit `[r, g, b]`。
///
/// 使用 [`DEFAULT_GAIN`] + 硬裁剪（与 Windows 自带 HDR 截图行为一致）。
/// `sdr_white_scrgb` 为 scRGB 归一化因子（= `SdrWhiteLevelInNits / 80`）。
pub fn hdr_to_srgb(r: f32, g: f32, b: f32, sdr_white_scrgb: f32) -> [u8; 3] {
    hdr_to_srgb_ex(r, g, b, sdr_white_scrgb, DEFAULT_GAIN, 1.0, 0.0)
}

/// 保色相 HDR → SDR 转换（完整参数版），输出 sRGB 8-bit `[r, g, b]`。
///
/// 步骤：按 SDR 白点归一化 → 算亮度 Y → 仅对 Y 做 [`gain_map`] 映射 →
/// 把缩放系数应用回三通道 → gamma 编码。
/// `gain` 为线性增益；`knee` / `max` 为增益后线性域的肩部起点/终点，
/// `max <= knee` 时硬裁剪。
pub fn hdr_to_srgb_ex(
    r: f32,
    g: f32,
    b: f32,
    sdr_white_scrgb: f32,
    gain: f32,
    knee: f32,
    max: f32,
) -> [u8; 3] {
    let rn = r / sdr_white_scrgb;
    let gn = g / sdr_white_scrgb;
    let bn = b / sdr_white_scrgb;
    let y = LUM_R * rn + LUM_G * gn + LUM_B * bn;
    let y_mapped = gain_map(y.max(0.0), gain, knee, max);
    // y 极小时（含负值）不做缩放，避免 scale 为负导致颜色反转
    let scale = if y > 1e-6 { y_mapped / y } else { 1.0 };
    let to_byte = |v: f32| {
        (linear_to_srgb_gamma((v * scale).clamp(0.0, 1.0)) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    [to_byte(rn), to_byte(gn), to_byte(bn)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两值近似相等（浮点容差）。
    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn sdr_linear_to_srgb_endpoints() {
        assert_eq!(sdr_linear_to_srgb(0.0, 0.0, 0.0), [0, 0, 0]);
        assert_eq!(sdr_linear_to_srgb(1.0, 1.0, 1.0), [255, 255, 255]);
    }

    #[test]
    fn sdr_linear_to_srgb_mid_gray() {
        // 0.5 线性灰 → gamma 编码 ≈ 0.7354 → 188（直通无增益，对比 hdr 路径的 147）
        let [r, g, b] = sdr_linear_to_srgb(0.5, 0.5, 0.5);
        assert_eq!(r, g);
        assert_eq!(g, b);
        assert!((187..=189).contains(&r), "0.5 线性灰应约 188，实际 {r}");
    }

    #[test]
    fn sdr_linear_to_srgb_clamps() {
        // 越界值 clamp 到 [0,1] 后再编码，不产生越界字节
        let [r, g, b] = sdr_linear_to_srgb(-0.5, 0.5, 1.5);
        assert_eq!(r, 0);
        assert!((187..=189).contains(&g));
        assert_eq!(b, 255);
    }

    #[test]
    fn gain_map_hard_clip_when_no_shoulder() {
        // max <= knee：纯增益 + 硬裁剪
        assert!(approx(gain_map(0.5, 0.617, 1.0, 0.0), 0.5 * 0.617));
        assert!(approx(gain_map(2.0, 0.617, 1.0, 0.0), 1.0));
        assert!(approx(gain_map(0.0, 0.617, 1.0, 0.0), 0.0));
    }

    #[test]
    fn gain_map_shoulder_identity_below_knee() {
        // 肩部起点（增益后 0.9）以下：输出 = y * gain，对比度无损
        assert!(approx(gain_map(0.5, 0.7, 0.9, 1.5), 0.35));
        assert!(approx(gain_map(1.0, 0.7, 0.9, 1.5), 0.7));
    }

    #[test]
    fn gain_map_shoulder_compresses_and_reaches_white() {
        // 肩部区间内被压缩但 > knee；max 处达满白；超过 max 饱和
        let y = gain_map(1.6, 0.7, 0.9, 1.5); // y*gain = 1.12 ∈ (0.9, 1.5)
        assert!(y > 0.9 && y < 1.0, "y={y}");
        assert!(approx(gain_map(1.5 / 0.7, 0.7, 0.9, 1.5), 1.0));
        assert!(approx(gain_map(100.0, 0.7, 0.9, 1.5), 1.0));
    }

    #[test]
    fn gain_map_monotonic() {
        let mut prev = 0.0f32;
        let mut x = 0.0f32;
        while x <= 20.0 {
            let y = gain_map(x, 0.7, 0.9, 1.5);
            assert!(y >= prev, "非单调：x={x} y={y} < prev={prev}");
            prev = y;
            x += 0.05;
        }
    }

    #[test]
    fn gain_map_preserves_local_contrast_below_knee() {
        // 肩部以下任意两点的"增益后比值"应与输入比值一致（线性增益 = 对比度平移）
        let a = gain_map(0.4, 0.7, 0.9, 1.5);
        let b = gain_map(0.8, 0.7, 0.9, 1.5);
        assert!(approx(a / b, 0.5));
    }

    #[test]
    fn gamma_endpoints() {
        assert!(approx(linear_to_srgb_gamma(0.0), 0.0));
        assert!(approx(linear_to_srgb_gamma(1.0), 1.0));
    }

    #[test]
    fn hdr_to_srgb_sdr_white_lands_at_windows_reference() {
        // Windows 实测：SDR 白点（归一化 1.0）经默认增益 → sRGB 206
        let [r, g, b] = hdr_to_srgb(1.0, 1.0, 1.0, 1.0);
        assert_eq!(r, g);
        assert_eq!(g, b);
        assert!((203..=209).contains(&r), "SDR 白点应落在 206 附近，实际 {r}");
    }

    #[test]
    fn hdr_to_srgb_gray_balance() {
        // 灰平衡：R=G=B 时输出三通道相等
        let [r, g, b] = hdr_to_srgb(0.5, 0.5, 0.5, 1.0);
        assert_eq!(r, g);
        assert_eq!(g, b);
    }

    #[test]
    fn hdr_to_srgb_preserves_hue_direction() {
        // 保色相：r > g == b 的输入，输出仍满足 r >= g 且 g == b
        let [r, g, b] = hdr_to_srgb(2.0, 0.5, 0.5, 1.0);
        assert!(r >= g);
        assert_eq!(g, b);
    }

    #[test]
    fn hdr_to_srgb_clips_highlights_to_white() {
        // 归一化亮度超过 1/gain（硬裁剪点）→ 满白
        let [r, _g, _b] = hdr_to_srgb(2.0, 2.0, 2.0, 1.0);
        assert_eq!(r, 255);
    }

    #[test]
    fn hdr_to_srgb_clamps_out_of_gamut() {
        // 负值 / 极大值不产生越界字节
        let [r, g, b] = hdr_to_srgb(-0.04, 0.5, 100.0, 2.0);
        assert_eq!(r, 0);
        assert!(g > 0 && g < 255, "中灰不应被裁到两端");
        assert_eq!(b, 255);
    }

    #[test]
    fn hdr_to_srgb_ex_shoulder_softens_clip() {
        // 带肩部：肩部区间内输出低于硬裁剪结果但仍单调到满白
        let hard = hdr_to_srgb_ex(1.4, 1.4, 1.4, 1.0, 0.7, 1.0, 0.0)[0];
        let soft = hdr_to_srgb_ex(1.4, 1.4, 1.4, 1.0, 0.7, 0.9, 1.5)[0];
        // 1.4 * 0.7 = 0.98，硬裁剪下 < 1.0 未满白；肩部版被压到更低
        assert!(soft < hard);
        // 超过肩部终点（1.5/0.7 ≈ 2.14）两者都为满白
        assert_eq!(hdr_to_srgb_ex(3.0, 3.0, 3.0, 1.0, 0.7, 0.9, 1.5)[0], 255);
    }
}
