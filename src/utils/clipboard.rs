//! 剪贴板读写封装（仅 Windows 平台编译）。
//!
//! 基于 `arboard`（其 Windows 实现同时写入 CF_DIBV5 与 PNG 格式，
//! 微信/Office 等只认特定格式的场景兼容性有保障，见 AGENTS.md 3.1）。
//!
//! 注意：Windows 剪贴板 API 需在 UI 线程调用（spike 阶段 `main` 直接调用即可）。

use std::borrow::Cow;

use anyhow::Context;
use image::RgbaImage;

/// 把 RGBA 图像复制到系统剪贴板。
///
/// # Errors
/// 打开剪贴板或写入失败时返回错误。
pub fn copy_image(img: &RgbaImage) -> anyhow::Result<()> {
    let mut clipboard =
        arboard::Clipboard::new().context("打开剪贴板失败（可能被其他程序占用）")?;
    let (width, height) = img.dimensions();
    let image_data = arboard::ImageData {
        width: width as usize,
        height: height as usize,
        bytes: Cow::Borrowed(img.as_raw()),
    };
    clipboard
        .set_image(image_data)
        .context("写入剪贴板失败")?;
    Ok(())
}

/// 把 UTF-8 文本复制到系统剪贴板（后续「提取文字一键复制」复用）。
///
/// # Errors
/// 打开剪贴板或写入失败时返回错误。
pub fn copy_text(text: &str) -> anyhow::Result<()> {
    let mut clipboard =
        arboard::Clipboard::new().context("打开剪贴板失败（可能被其他程序占用）")?;
    clipboard.set_text(text).context("写入剪贴板失败")?;
    Ok(())
}
