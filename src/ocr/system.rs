//! 系统 OCR 引擎（`Windows.Media.Ocr`，内置兜底，见 AGENTS.md 3.8 节）。
//!
//! - 引擎：优先用户配置语言（`TryCreateFromUserProfileLanguages`），
//!   失败回退简体中文（`TryCreateFromLanguage("zh-Hans")`）；
//! - 输入：截图 RGBA 经 `DataWriter` 灌 BGRA 字节 → `SoftwareBitmap`
//!   （`Bgra8 + Premultiplied`，截图不透明时与原图一致）；
//! - 输出：`OcrLine` 按行转 [`TextRegion`]（词框外接 + 词间智能空格），
//!   超 `MaxImageDimension` 等比缩小后坐标回映射；
//! - WinRT OCR **不返回置信度**，`confidence` 恒填 `1.0`
//!   （此时 Auto 退化为纯模式二，分流红利需 Rapid 插件，见 3.8 节）。
//!
//! 同步阻塞调用（`IAsyncOperation::join`），调用方必须包在 `spawn_blocking`
//! 里（见工具条后台任务），勿阻塞 UI 线程。

use std::sync::OnceLock;

use windows::core::HSTRING;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine as WinOcrEngine;
use windows::Storage::Streams::DataWriter;

use super::{join_words, OcrEngine, TextRegion};
use crate::ocr::BBox;

/// 系统 OCR 引擎（零额外分发，开箱即用）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemOcrEngine;

/// 行框补白比例（行高的 15%，四向；对齐 Rapid unclip 外扩的思想，
/// 但固定比例更可预测，见 `recognize_impl`；本文件仅 Windows 编译）。
const BOX_PAD_RATIO: f32 = 0.15;

/// 引擎可用性缓存（WinRT 引擎创建开销小但无状态变化时没必要反复查；
/// 语言包是系统级安装，运行时不变，进程内缓存合理）。
static AVAILABLE: OnceLock<bool> = OnceLock::new();

impl SystemOcrEngine {
    /// 创建 WinRT OCR 引擎（用户语言优先，失败回退简体中文）。
    fn create_engine() -> anyhow::Result<WinOcrEngine> {
        if let Ok(engine) = WinOcrEngine::TryCreateFromUserProfileLanguages() {
            return Ok(engine);
        }
        let zh = Language::CreateLanguage(&HSTRING::from("zh-Hans"))?;
        WinOcrEngine::TryCreateFromLanguage(&zh).map_err(|e| {
            anyhow::anyhow!("系统 OCR 不可用（可能未安装中文 OCR 语言包：设置→时间和语言→语言和区域→中文→语言选项→光学字符识别）: {e:#}")
        })
    }

    /// 识别一张图（内部：缩放钳制 → SoftwareBitmap → RecognizeAsync → 按行组装）。
    fn recognize_impl(image: &image::DynamicImage) -> anyhow::Result<Vec<TextRegion>> {
        let engine = Self::create_engine()?;
        // MaxImageDimension 超限等比缩小（WinRT 硬限制，不缩直接报错）
        let max_side = WinOcrEngine::MaxImageDimension().unwrap_or(4096);
        let (rgba, scale) = fit_to_limit(&image.to_rgba8(), max_side);
        let bitmap = rgba_to_bitmap(&rgba)?;
        // 同步阻塞等完成（`IAsyncOperation::join`；调用方必须包在 spawn_blocking
        // 里，勿阻塞 UI 线程；WinRT 内部走系统线程池，不占 tokio）。
        let result = engine.RecognizeAsync(&bitmap)?.join()?;
        let lines = result.Lines()?;
        let mut out = Vec::new();
        for li in 0..lines.Size()? {
            let line = lines.GetAt(li)?;
            let words = line.Words()?;
            if words.Size()? == 0 {
                continue;
            }
            // 词框外接 + 文本拼接（CJK 相邻不加空格，见 join_words）
            let mut x0 = u32::MAX;
            let mut y0 = u32::MAX;
            let mut x1 = 0u32;
            let mut y1 = 0u32;
            let mut texts: Vec<String> = Vec::new();
            for wi in 0..words.Size()? {
                let word = words.GetAt(wi)?;
                let r = word.BoundingRect()?;
                // 回映射到原图坐标（等比缩放逆变换）
                let (wx0, wy0, wx1, wy1) = (
                    (r.X.max(0.0) / scale) as u32,
                    (r.Y.max(0.0) / scale) as u32,
                    ((r.X + r.Width).max(0.0) / scale) as u32,
                    ((r.Y + r.Height).max(0.0) / scale) as u32,
                );
                x0 = x0.min(wx0);
                y0 = y0.min(wy0);
                x1 = x1.max(wx1);
                y1 = y1.max(wy1);
                texts.push(word.Text()?.to_string());
            }
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            // WinRT 词框是紧油墨框（无 Rapid 那样的 unclip 外扩），直接用会
            // 让字号反推偏小、排版余量不足。按行高 15% 四向补白（钳制在图内），
            // 补完的高度再反推字号（2026-09-06 实机：系统 OCR 小字）。
            let (img_w, img_h) = (image.width(), image.height());
            let pad = ((y1 - y0) as f32 * BOX_PAD_RATIO).round() as u32;
            let (px0, py0) = (x0.saturating_sub(pad), y0.saturating_sub(pad));
            let (px1, py1) =
                ((x1 + pad).min(img_w), (y1 + pad).min(img_h));
            if px1 <= px0 || py1 <= py0 {
                continue;
            }
            let height = py1 - py0;
            out.push(TextRegion {
                id: out.len(),
                bbox: BBox { x: px0, y: py0, width: px1 - px0, height },
                text: Some(join_words(&texts)),
                confidence: 1.0,
                angle: 0.0,
                est_font_size: TextRegion::estimate_font_size(height),
            });
        }
        Ok(out)
    }
}

impl OcrEngine for SystemOcrEngine {
    fn name(&self) -> &'static str {
        "system"
    }

    fn detect(&self, image: &image::DynamicImage) -> anyhow::Result<Vec<TextRegion>> {
        // WinRT 识别与检测一体：复用识别结果，仅丢弃文字（供模式一裁剪用）。
        Ok(Self::recognize_impl(image)?
            .into_iter()
            .map(|mut r| {
                r.text = None;
                r
            })
            .collect())
    }

    fn detect_and_recognize(
        &self,
        image: &image::DynamicImage,
    ) -> anyhow::Result<Vec<TextRegion>> {
        Self::recognize_impl(image)
    }

    fn is_available(&self) -> bool {
        *AVAILABLE.get_or_init(|| Self::create_engine().is_ok())
    }
}

/// 长边超限等比缩小（返回缩小后图像 + 缩放比；未超限时缩放比为 1.0）。
fn fit_to_limit(
    rgba: &image::RgbaImage,
    max_side: u32,
) -> (image::RgbaImage, f32) {
    let (w, h) = (rgba.width(), rgba.height());
    let long = w.max(h);
    if long <= max_side || long == 0 {
        return (rgba.clone(), 1.0);
    }
    let scale = max_side as f32 / long as f32;
    let (nw, nh) = ((w as f32 * scale) as u32, (h as f32 * scale) as u32);
    (
        image::imageops::resize(rgba, nw.max(1), nh.max(1), image::imageops::FilterType::Triangle),
        scale,
    )
}

/// RGBA 转 WinRT SoftwareBitmap（经 DataWriter 灌 BGRA 字节）。
fn rgba_to_bitmap(rgba: &image::RgbaImage) -> anyhow::Result<SoftwareBitmap> {
    let (w, h) = (rgba.width(), rgba.height());
    if w == 0 || h == 0 {
        anyhow::bail!("空图像无法识别");
    }
    // RGBA → BGRA（R/B 通道互换；截图不透明，premultiplied 与原图一致）
    let mut bgra = Vec::with_capacity((w * h * 4) as usize);
    for p in rgba.pixels() {
        bgra.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
    }
    let writer = DataWriter::new()?;
    writer.WriteBytes(&bgra)?;
    let buffer = writer.DetachBuffer()?;
    // 注：UWP 的 CreateCopyFromBuffer 只有 4 参（无 alpha 形参），BGRA 数据
    // 按预乘理解；截图不透明，premultiplied 与原图一致，无需 WithAlpha 变体。
    Ok(SoftwareBitmap::CreateCopyFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        w as i32,
        h as i32,
    )?)
}
