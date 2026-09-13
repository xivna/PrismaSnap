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
use std::borrow::Cow;

/// 捕获帧像素格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawFrameFormat {
    /// scRGB 线性 f16（HDR 路径；每像素 8 字节）。
    Rgba16F,
    /// sRGB 编码 8-bit（SDR 路径；每像素 4 字节）。WGC `Rgba8` 已是 gamma
    /// 编码值，不能再走 linear→sRGB，否则双重 gamma。
    Rgba8,
}

/// 从捕获线程传回的一帧原始数据。
///
/// `data` 按行排列，`row_pitch` 可能大于每像素行字节数（对齐），遍历时须按
/// `row_pitch` 计算行偏移。
pub struct RawFrame {
    /// 帧宽度（像素）。
    pub width: u32,
    /// 帧高度（像素）。
    pub height: u32,
    /// 每行字节数（含对齐填充）。
    pub row_pitch: usize,
    /// 原始像素数据（格式见 [`Self::format`]）。
    pub data: Vec<u8>,
    /// 像素格式。
    pub format: RawFrameFormat,
    /// 来源显示器的 GDI 设备名（如 `\\.\DISPLAY1`），用于查询 SDR 白点。
    pub device_name: String,
}

impl RawFrame {
    /// 把 Rgba16F 帧数据紧凑化为连续布局，行按 256 字节对齐（行尾补零）。
    ///
    /// WGC 返回的行距 `row_pitch` 可能带对齐填充，且不保证是 256 的倍数；
    /// wgpu 的 `write_texture` 要求 `bytes_per_row` 为 256 的倍数。
    /// 因此每行取前 `width * 8` 字节有效数据紧凑排列，行尾补零到 256 对齐
    /// （padding 在像素区之外，采样不会触达）。
    ///
    /// 快道（2026-09-12 性能优化）：常见分辨率下 `row_pitch` 本来就等于
    /// `width * 8`（无填充，如 3840×8 = 30720 = 256×120），此时直接借用原
    /// 数据零拷贝返回，省掉一次全帧（4K 约 66MB）分配 + 拷贝 + 置零。
    pub fn compact_rgba16f_data(&self) -> Cow<'_, [u8]> {
        let bytes_per_row = self.width as usize * 8;
        let h = self.height as usize;
        if self.row_pitch == bytes_per_row && bytes_per_row.is_multiple_of(256) {
            let len = bytes_per_row.saturating_mul(h).min(self.data.len());
            return Cow::Borrowed(&self.data[..len]);
        }
        let aligned = bytes_per_row.next_multiple_of(256);
        let mut out = vec![0u8; aligned * h];
        for y in 0..h {
            let src = y * self.row_pitch;
            let dst = y * aligned;
            out[dst..dst + bytes_per_row]
                .copy_from_slice(&self.data[src..src + bytes_per_row]);
        }
        Cow::Owned(out)
    }
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

/// 把 WGC `Rgba8` 帧拷成图像。数据已是 sRGB 编码，不再做 gamma。
///
/// 行距可能带对齐填充，按 `width * 4` 逐行紧凑拷贝。
pub fn frame_rgba8_to_image(frame: &RawFrame) -> RgbaImage {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let dst_stride = w * 4;
    let mut buf = vec![0u8; dst_stride * h];
    for y in 0..h {
        let src = y * frame.row_pitch;
        let dst = y * dst_stride;
        buf[dst..dst + dst_stride].copy_from_slice(&frame.data[src..src + dst_stride]);
    }
    match RgbaImage::from_raw(frame.width, frame.height, buf) {
        Some(img) => img,
        None => {
            tracing::error!("Rgba8 帧尺寸不匹配，回退空图");
            RgbaImage::new(frame.width, frame.height)
        }
    }
}

/// 按逐像素转换函数把 Rgba16F 帧转成 RGBA8 图像（两公开函数共用）。
///
/// 性能（2026-09-12 优化）：4K 全帧逐像素 `powf` 约 300ms+，是"热键→覆盖层
/// 出现"链路上可压缩的一段。两处优化，均与旧逻辑逐字节一致：
/// - 直接写 `Vec<u8>` 缓冲（旧 `put_pixel` 逐像素边界检查）；
/// - 按行分带 `std::thread::scope` 并行（无新依赖，线程数按可用核心钳制，
///   小图退化为单线程，转换闭包只要求多加 `Sync` 界）。
fn convert_frame(
    frame: &RawFrame,
    convert: impl Fn(f32, f32, f32) -> [u8; 3] + Sync + Send,
) -> RgbaImage {
    let w = frame.width as usize;
    let h = frame.height as usize;
    if w == 0 || h == 0 {
        return RgbaImage::new(frame.width, frame.height);
    }
    let mut buf = vec![0u8; w * h * 4];
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 32)
        .min(h);
    if threads <= 1 {
        convert_rows(frame, &convert, &mut buf, 0, h);
    } else {
        let rows_per_band = h.div_ceil(threads);
        let mut bands = Vec::with_capacity(threads);
        let mut rest = buf.as_mut_slice();
        let mut y0 = 0;
        while y0 < h {
            let y1 = (y0 + rows_per_band).min(h);
            let (band, tail) = rest.split_at_mut((y1 - y0) * w * 4);
            bands.push((band, y0, y1));
            rest = tail;
            y0 = y1;
        }
        std::thread::scope(|s| {
            let convert_ref = &convert;
            for (band, y0, y1) in bands {
                s.spawn(move || convert_rows(frame, convert_ref, band, y0, y1));
            }
        });
    }
    match RgbaImage::from_raw(frame.width, frame.height, buf) {
        Some(img) => img,
        None => {
            tracing::error!("转换缓冲尺寸不匹配，回退空图");
            RgbaImage::new(frame.width, frame.height)
        }
    }
}

/// 转换行带 `[y0, y1)`（`out` 恰为该行带像素区，不含填充）。
fn convert_rows(
    frame: &RawFrame,
    convert: &(impl Fn(f32, f32, f32) -> [u8; 3] + Sync + Send),
    out: &mut [u8],
    y0: usize,
    y1: usize,
) {
    let w = frame.width as usize;
    for y in y0..y1 {
        let row_start = y * frame.row_pitch;
        let out_row = &mut out[(y - y0) * w * 4..][..w * 4];
        for x in 0..w {
            let px = row_start + x * 8;
            let r = read_f16(&frame.data[px..px + 2]);
            let g = read_f16(&frame.data[px + 2..px + 4]);
            let b = read_f16(&frame.data[px + 4..px + 6]);
            let a = read_f16(&frame.data[px + 6..px + 8]);

            let [sr, sg, sb] = convert(r, g, b);
            let alpha = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
            let o = x * 4;
            out_row[o] = sr;
            out_row[o + 1] = sg;
            out_row[o + 2] = sb;
            out_row[o + 3] = alpha;
        }
    }
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
            format: RawFrameFormat::Rgba16F,
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

    #[test]
    fn compact_rgba16f_data_strips_padding_and_aligns() {
        // 2 像素宽的帧，row_pitch = 24（含 8 字节行尾填充）：
        // 行 0 有效数据 16 字节 + 8 字节填充，行 1 同。构造时逐字节写入以区分填充。
        let mut data = vec![0xFFu8; 24 * 2];
        for y in 0..2 {
            for i in 0..16 {
                data[y * 24 + i] = (y * 16 + i) as u8;
            }
            // 行尾 8 字节填充保持 0xFF
        }
        let frame = RawFrame {
            width: 2,
            height: 2,
            row_pitch: 24,
            data,
            format: RawFrameFormat::Rgba16F,
            device_name: String::from("test"),
        };
        let compact = frame.compact_rgba16f_data();
        // 每行对齐 256 字节，共 2 行
        assert_eq!(compact.len(), 256 * 2);
        for y in 0..2 {
            let row = &compact[y * 256..y * 256 + 16];
            for (i, &b) in row.iter().enumerate() {
                assert_eq!(b, (y * 16 + i) as u8, "行 {y} 字节 {i} 与源数据不一致");
            }
            // 行尾补零（而非填充字节 0xFF）
            assert!(compact[y * 256 + 16..(y + 1) * 256].iter().all(|&b| b == 0));
        }
    }

    #[test]
    fn frame_rgba8_to_image_strips_pitch_padding() {
        // 宽 2：有效 8 字节/行，row_pitch=12（4 字节填充）
        let mut data = vec![0u8; 12 * 2];
        data[0..8].copy_from_slice(&[10, 20, 30, 255, 40, 50, 60, 255]);
        data[12..20].copy_from_slice(&[70, 80, 90, 255, 100, 110, 120, 255]);
        let frame = RawFrame {
            width: 2,
            height: 2,
            row_pitch: 12,
            data,
            format: RawFrameFormat::Rgba8,
            device_name: String::from("test"),
        };
        let img = frame_rgba8_to_image(&frame);
        assert_eq!(img.get_pixel(0, 0).0, [10, 20, 30, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [40, 50, 60, 255]);
        assert_eq!(img.get_pixel(0, 1).0, [70, 80, 90, 255]);
        assert_eq!(img.get_pixel(1, 1).0, [100, 110, 120, 255]);
    }

    #[test]
    fn compact_rgba16f_data_pitch_equals_row() {
        // 无填充时（row_pitch == width*8）紧凑化结果 = 原数据 + 行对齐补零
        let frame = make_frame([0.25, 0.5, 1.0]);
        let compact = frame.compact_rgba16f_data();
        assert_eq!(compact.len(), 256);
        assert_eq!(&compact[..16], &frame.data[..16]);
        assert!(compact[16..].iter().all(|&b| b == 0));
    }
}
