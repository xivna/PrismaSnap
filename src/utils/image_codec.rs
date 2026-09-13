//! PNG/JPEG 编解码封装（跨平台，基于 `image` crate）。

use std::path::Path;

use anyhow::Context;
use image::RgbaImage;

/// 把 RGBA 图像保存为 PNG 文件（无损，截图默认格式）。
///
/// # Errors
/// 编码或文件写入失败时返回错误（`anyhow` 带上下文）。
pub fn save_png(img: &RgbaImage, path: &Path) -> anyhow::Result<()> {
    img.save(path)
        .with_context(|| format!("保存 PNG 失败: {}", path.display()))
}

/// 把 RGBA 图像保存为 JPEG 文件（有损，不支持透明通道，内部丢弃 alpha）。
///
/// * `quality` - 编码质量 1~100（越高体积越大、细节越好）。
///
/// # Errors
/// 编码或文件写入失败时返回错误。
pub fn save_jpeg(img: &RgbaImage, path: &Path, quality: u8) -> anyhow::Result<()> {
    // JPEG 编码器不支持 RGBA8，先丢弃 alpha 转为 RGB8
    let rgb = image::RgbImage::from_fn(img.width(), img.height(), |x, y| {
        let p = img.get_pixel(x, y);
        image::Rgb([p[0], p[1], p[2]])
    });
    let mut writer = std::io::BufWriter::new(
        std::fs::File::create(path)
            .with_context(|| format!("创建文件失败: {}", path.display()))?,
    );
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, quality);
    rgb.write_with_encoder(encoder)
        .with_context(|| format!("编码 JPEG 失败: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个 2x2 测试图像。
    fn test_img() -> RgbaImage {
        let mut img = RgbaImage::new(2, 2);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgba([(x * 100) as u8, (y * 100) as u8, 128, 255]);
        }
        img
    }

    #[test]
    fn save_and_load_png_roundtrip() {
        let img = test_img();
        let dir = std::env::temp_dir();
        let path = dir.join("prismsnap_codec_test.png");
        save_png(&img, &path).unwrap();
        let loaded = image::open(&path).unwrap().to_rgba8();
        assert_eq!(loaded.width(), 2);
        assert_eq!(loaded.height(), 2);
        assert_eq!(loaded.get_pixel(1, 1), &image::Rgba([100, 100, 128, 255]));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_jpeg_works() {
        let img = test_img();
        let dir = std::env::temp_dir();
        let path = dir.join("prismsnap_codec_test.jpg");
        save_jpeg(&img, &path, 90).unwrap();
        let loaded = image::open(&path).unwrap().to_rgba8();
        assert_eq!((loaded.width(), loaded.height()), (2, 2));
        let _ = std::fs::remove_file(&path);
    }
}
