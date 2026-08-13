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
//! 经外部咨询（见 docs/关于HDR色彩转换技术方案与问题的回复.md）确定的方案：
//! - **knee/shoulder 曲线**：knee 以下斜率恒为 1（原样输出），只在
//!   knee 到 headroom 之间做平滑压缩——取代早期"全局线性压暗"（发灰根源）。
//! - **保色相**：只对亮度 Y 做 tone map，把同一缩放系数应用回 RGB 三通道，
//!   避免三个通道各自压缩比例不一致导致的色相偏移。

/// Rec.709 亮度加权系数（scRGB/sRGB 均采用该组系数）。
pub const LUM_R: f32 = 0.2126;
pub const LUM_G: f32 = 0.7152;
pub const LUM_B: f32 = 0.0722;

/// knee/shoulder 曲线默认 knee 点（归一化亮度，SDR 白点 = 1.0）。
///
/// 外部建议 0.6 ~ 0.75 之间，取 0.7 折中；实机观感不佳可再调。
pub const DEFAULT_KNEE: f32 = 0.7;

/// 查询不到显示器实际 headroom 时的兜底值（headroom = 最大亮度 / SDR 白点）。
pub const FALLBACK_HEADROOM: f32 = 8.0;

/// knee/shoulder 色调映射曲线。
///
/// 入参 `x` 为已按 SDR 白点归一化后的线性亮度（SDR 白点 = 1.0）。
/// - `x <= knee`：恒等映射（斜率 1），保证 SDR 中低亮度内容"所见即所得"；
/// - `x > knee`：以 ease-out 平滑压缩到 `[knee, 1.0]`，`x == headroom` 时达满白。
///
/// `headroom` 应取"显示器最大亮度 / SDR 白点"（物理准确），而非拍脑袋常数；
/// 必须大于 `knee`，否则 `span` 退化为极小正数兜底。
pub fn tone_map(x: f32, knee: f32, headroom: f32) -> f32 {
    if x <= knee {
        x
    } else {
        let span = (headroom - knee).max(1e-4);
        let t = ((x - knee) / span).min(1.0);
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

/// 黑位校正：把暗部参考点 `black_ref` 拉回 0，并重新拉伸到 `[0, 1]`。
///
/// 用于修正 WGC scRGB 数据里 SDR 黑位被抬升（画面"发灰"、暗部发雾）的问题。
/// `x` 为归一化后的线性值，`black_ref` 为暗部参考（如最暗 0.1% 像素的亮度）。
/// `black_ref <= 0` 或 `>= 1` 时不作校正，原样返回。
pub fn black_point_correct(x: f32, black_ref: f32) -> f32 {
    if black_ref <= 0.0 || black_ref >= 1.0 {
        x
    } else {
        ((x - black_ref) / (1.0 - black_ref)).max(0.0)
    }
}

/// 保色相 HDR → SDR 转换，输出 sRGB 8-bit `[r, g, b]`。
///
/// 步骤：按 SDR 白点归一化 → 算亮度 Y → 仅对 Y 做 tone map → 把缩放系数
/// 应用回三通道 → gamma 编码。`sdr_white_scrgb` 为 scRGB 归一化因子
/// （= `SdrWhiteLevelInNits / 80`）。
pub fn hdr_to_srgb(
    r: f32,
    g: f32,
    b: f32,
    sdr_white_scrgb: f32,
    knee: f32,
    headroom: f32,
) -> [u8; 3] {
    hdr_to_srgb_inner(r, g, b, sdr_white_scrgb, knee, headroom, 0.0)
}

/// 带黑位校正的保色相 HDR → SDR 转换。
///
/// 与 [`hdr_to_srgb`] 相同，但在归一化后先做 [`black_point_correct`]，
/// 用于修正暗部发灰。`black_ref` 为暗部参考亮度（0 = 不校正）。
pub fn hdr_to_srgb_with_black_point(
    r: f32,
    g: f32,
    b: f32,
    sdr_white_scrgb: f32,
    knee: f32,
    headroom: f32,
    black_ref: f32,
) -> [u8; 3] {
    hdr_to_srgb_inner(r, g, b, sdr_white_scrgb, knee, headroom, black_ref)
}

/// [`hdr_to_srgb`] 的内部实现。
fn hdr_to_srgb_inner(
    r: f32,
    g: f32,
    b: f32,
    sdr_white_scrgb: f32,
    knee: f32,
    headroom: f32,
    black_ref: f32,
) -> [u8; 3] {
    let mut rn = r / sdr_white_scrgb;
    let mut gn = g / sdr_white_scrgb;
    let mut bn = b / sdr_white_scrgb;
    if black_ref > 0.0 {
        rn = black_point_correct(rn, black_ref);
        gn = black_point_correct(gn, black_ref);
        bn = black_point_correct(bn, black_ref);
    }
    let y = LUM_R * rn + LUM_G * gn + LUM_B * bn;
    let y_mapped = tone_map(y.max(0.0), knee, headroom);
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
    fn tone_map_identity_below_knee() {
        // knee 以下恒等映射
        assert!(approx(tone_map(0.0, 0.7, 8.0), 0.0));
        assert!(approx(tone_map(0.5, 0.7, 8.0), 0.5));
        assert!(approx(tone_map(0.7, 0.7, 8.0), 0.7));
    }

    #[test]
    fn tone_map_compresses_above_knee() {
        // 高于 knee 被压缩，但严格大于 knee
        let y = tone_map(2.0, 0.7, 8.0);
        assert!(y > 0.7 && y < 2.0);
    }

    #[test]
    fn tone_map_reaches_white_at_headroom() {
        // x == headroom 时达满白
        assert!(approx(tone_map(8.0, 0.7, 8.0), 1.0));
        // 超过 headroom 饱和到 1.0
        assert!(approx(tone_map(100.0, 0.7, 8.0), 1.0));
    }

    #[test]
    fn tone_map_monotonic() {
        let mut prev = 0.0f32;
        let mut x = 0.0f32;
        while x <= 20.0 {
            let y = tone_map(x, 0.7, 8.0);
            assert!(y >= prev, "非单调：x={x} y={y} < prev={prev}");
            prev = y;
            x += 0.05;
        }
    }

    #[test]
    fn tone_map_handles_headroom_close_to_knee() {
        // headroom 接近 knee 时 span 兜底，不 panic、不产生 NaN
        let y = tone_map(1.0, 0.7, 0.7);
        assert!(y.is_finite() && y >= 0.7 && y <= 1.0);
    }

    #[test]
    fn gamma_endpoints() {
        assert!(approx(linear_to_srgb_gamma(0.0), 0.0));
        assert!(approx(linear_to_srgb_gamma(1.0), 1.0));
    }

    #[test]
    fn hdr_to_srgb_gray_balance() {
        // 灰平衡：R=G=B 时输出三通道相等
        let [r, g, b] = hdr_to_srgb(0.5, 0.5, 0.5, 1.0, 0.7, 8.0);
        assert_eq!(r, g);
        assert_eq!(g, b);
    }

    #[test]
    fn hdr_to_srgb_preserves_hue_direction() {
        // 保色相：r > g == b 的输入，输出仍满足 r >= g 且 g == b
        let [r, g, b] = hdr_to_srgb(2.0, 0.5, 0.5, 1.0, 0.7, 8.0);
        assert!(r >= g);
        assert_eq!(g, b);
    }

    #[test]
    fn hdr_to_srgb_sdr_white_lands_near_knee() {
        // SDR 白点（归一化 = 1.0）落在 knee~1.0 之间（轻微压暗为高光留空间）
        let [r, _g, _b] = hdr_to_srgb(1.0, 1.0, 1.0, 1.0, 0.7, 8.0);
        assert!(r < 255, "SDR 白点不应直接满白");
        assert!(r > 200, "SDR 白点不应过度压暗");
    }

    #[test]
    fn hdr_to_srgb_clamps_out_of_gamut() {
        // 负值 / 极大值不产生越界字节
        let [r, g, b] = hdr_to_srgb(-0.04, 0.5, 100.0, 2.0, 0.7, 8.0);
        assert_eq!(r, 0);
        assert!(g > 0 && g < 255, "中灰不应被裁到两端");
        assert_eq!(b, 255);
    }

    #[test]
    fn black_point_correct_pulls_black_to_zero() {
        // 黑位抬升 0.02 时，最暗像素应被拉回 0
        assert!(approx(black_point_correct(0.02, 0.02), 0.0));
        // 黑位校正后白点（1.0）保持为 1.0
        assert!(approx(black_point_correct(1.0, 0.02), 1.0));
        // 低于黑位的值 clamp 到 0
        assert!(approx(black_point_correct(0.01, 0.02), 0.0));
    }

    #[test]
    fn black_point_correct_noop_when_black_is_zero() {
        // black_ref 为 0 时不做校正
        assert!(approx(black_point_correct(0.3, 0.0), 0.3));
        // black_ref 非法（>= 1）时不做校正
        assert!(approx(black_point_correct(0.3, 1.5), 0.3));
    }
}
