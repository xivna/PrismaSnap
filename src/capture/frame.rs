//! 捕获帧数据结构与 Rgba16F → sRGB 转换（跨平台纯逻辑，WSL2 可单测）。
//!
//! `windows-capture` 返回的 `Rgba16F` 数据是 IEEE754 f16、scRGB 线性色彩空间，
//! Rust 无原生 f16 类型，需经 `half` 转为 f32 后再做色彩转换
//! （见 AGENTS.md 3.1 / 3.2 节）。
//!
//! 转换方案已定稿（2026-08-13 实机对照实验）：
//! SDR 白点归一化 + 线性增益 + 硬裁剪 + 保色相，与 Windows 自带截图行为一致。
//! 具体实现见 [`crate::capture::color`]。

use half::f16;
use image::RgbaImage;

/// 从捕获线程传回的一帧原始数据（Rgba16F 格式）。
///
/// `data` 按行排列，`row_pitch` 可能大于 `width * 8`（对齐），遍历时须按
/// `row_pitch` 计算行偏移。每像素 4 通道（R/G/B/A），每通道 2 字节小端 f16。
pub struct RawFrame {
    /// 帧宽度（像素）。
    pub width: u32,
    /// 帧高度（像素）。
    pub height: u32,
    /// 每行字节数（含对齐填充）。
    pub row_pitch: usize,
    /// 原始像素数据（f16 小端字节流）。
    pub data: Vec<u8>,
    /// 来源显示器的 GDI 设备名（如 `\\.\DISPLAY1`），用于查询 SDR 白点。
    pub device_name: String,
}

/// 从 2 字节小端数据读出 f16 并转 f32。
///
/// # Panics
/// 当 `bytes` 长度不足 2 时 panic（内部循环保证不越界，不对外暴露）。
pub fn read_f16(bytes: &[u8]) -> f32 {
    debug_assert!(bytes.len() >= 2);
    f16::from_bits(u16::from_le_bytes([bytes[0], bytes[1]])).to_f32()
}

/// 把 Rgba16F 原始帧按定稿方案转换为 8-bit sRGB RGBA 图像。
///
/// * `sdr_white_scrgb` - SDR 白点的 scRGB 值（= 显示器 SDR 白点 nit / 80.0），
///   由 [`crate::capture::display_info::query_sdr_white_nits`] 查询得到。
///
/// alpha 通道做简单 clamp（屏幕捕获 alpha 恒为 1.0，仅作兼容处理）。
pub fn frame_to_srgb_image(frame: &RawFrame, sdr_white_scrgb: f32) -> RgbaImage {
    convert_frame(frame, |r, g, b| {
        crate::capture::color::hdr_to_srgb(r, g, b, sdr_white_scrgb)
    })
}

/// 把 Rgba16F 原始帧按 **SDR 直通**转换为 8-bit sRGB RGBA 图像。
///
/// 用于系统未开启 HDR 的显示器：数据本身就是 0~1.0 的线性 SDR，
/// 直接 gamma 编码即为原图（无增益、无归一化，见
/// [`crate::capture::color::sdr_linear_to_srgb`]）。
pub fn frame_to_srgb_image_direct(frame: &RawFrame) -> RgbaImage {
    convert_frame(frame, crate::capture::color::sdr_linear_to_srgb)
}

/// 按逐像素转换函数把 Rgba16F 帧转成 RGBA8 图像（两公开函数共用）。
fn convert_frame(frame: &RawFrame, convert: impl Fn(f32, f32, f32) -> [u8; 3]) -> RgbaImage {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let mut img = RgbaImage::new(frame.width, frame.height);

    for y in 0..h {
        let row_start = y * frame.row_pitch;
        for x in 0..w {
            let px = row_start + x * 8;
            let r = read_f16(&frame.data[px..px + 2]);
            let g = read_f16(&frame.data[px + 2..px + 4]);
            let b = read_f16(&frame.data[px + 4..px + 6]);
            let a = read_f16(&frame.data[px + 6..px + 8]);

            let [sr, sg, sb] = convert(r, g, b);
            let alpha = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
            img.put_pixel(x as u32, y as u32, image::Rgba([sr, sg, sb, alpha]));
        }
    }
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个宽 2、高 1 的紧凑 Rgba16F 帧（无填充，row_pitch = 16，alpha 恒 1.0）。
    fn make_frame(rgb: [f32; 3]) -> RawFrame {
        let mut data = Vec::with_capacity(16);
        // 两个像素：每像素 r/g/b + alpha=1.0
        for &v in rgb.iter() {
            data.extend_from_slice(&f16::from_f32(v).to_bits().to_le_bytes());
        }
        data.extend_from_slice(&f16::from_f32(1.0).to_bits().to_le_bytes());
        for &v in rgb.iter() {
            data.extend_from_slice(&f16::from_f32(v).to_bits().to_le_bytes());
        }
        data.extend_from_slice(&f16::from_f32(1.0).to_bits().to_le_bytes());
        RawFrame {
            width: 2,
            height: 1,
            row_pitch: 16,
            data,
            device_name: String::from("test"),
        }
    }

    #[test]
    fn read_f16_converts_back() {
        let bits = f16::from_f32(1.5).to_bits().to_le_bytes();
        assert!((read_f16(&bits) - 1.5).abs() < 1e-3);
    }

    #[test]
    fn frame_to_srgb_image_dimensions() {
        let frame = make_frame([0.0, 0.0, 0.0]);
        let img = frame_to_srgb_image(&frame, 1.0);
        assert_eq!(img.width(), 2);
        assert_eq!(img.height(), 1);
    }

    #[test]
    fn frame_to_srgb_image_passes_through_sdr_white() {
        // SDR 白点 = 1.0（归一化后不变），输入 0.5 灰 → 输出应约 0.735（sRGB gamma）
        let frame = make_frame([0.5, 0.5, 0.5]);
        let img = frame_to_srgb_image(&frame, 1.0);
        let p = img.get_pixel(0, 0);
        let expected = crate::capture::color::hdr_to_srgb(0.5, 0.5, 0.5, 1.0);
        assert_eq!([p[0], p[1], p[2]], expected);
        // alpha 恒 1.0 → 255
        assert_eq!(p[3], 255);
    }

    #[test]
    fn frame_to_srgb_image_direct_matches_color_fn() {
        // 直通版与 color::sdr_linear_to_srgb 输出一致，且 1.0 白不压暗（SDR 屏原图直出）
        let frame = make_frame([1.0, 0.5, 0.0]);
        let img = frame_to_srgb_image_direct(&frame);
        let p = img.get_pixel(0, 0);
        let expected = crate::capture::color::sdr_linear_to_srgb(1.0, 0.5, 0.0);
        assert_eq!([p[0], p[1], p[2]], expected);
        assert_eq!(p[0], 255, "纯白不应被压暗");
        assert_eq!(p[3], 255);
    }
}
