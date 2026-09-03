//! 系统 OCR 引擎（`Windows.Media.Ocr`，内置兜底，见 AGENTS.md 3.8 节）。
//!
//! 骨架占位：真正的 WinRT 调用（`OcrEngine::RecognizeAsync` 取 `SoftwareBitmap`，
//! `OcrWord::BoundingRect` 取词框后按行聚合成 `TextRegion`，`MaxImageDimension`
//! 超限等比缩放回映射）在 TASKS Phase 4 #3 落地。注意 WinRT OCR 不返回置信度，
//! 实现时 `confidence` 恒填 `1.0`（此时 Auto 退化为纯模式二）。
//!
//! `windows` 需新增特性：`Media_Ocr` + `Graphics_Imaging` + `Globalization` +
//! `Foundation`（见 AGENTS.md 3.1），随 #3 一并引入。

use super::{OcrEngine, TextRegion};

/// 系统 OCR 引擎（零额外分发，开箱即用）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemOcrEngine;

impl OcrEngine for SystemOcrEngine {
    fn name(&self) -> &'static str {
        "system"
    }

    fn detect(&self, _image: &image::DynamicImage) -> anyhow::Result<Vec<TextRegion>> {
        anyhow::bail!("SystemOcrEngine 尚未实现（TASKS Phase 4 #3）")
    }

    fn detect_and_recognize(
        &self,
        _image: &image::DynamicImage,
    ) -> anyhow::Result<Vec<TextRegion>> {
        anyhow::bail!("SystemOcrEngine 尚未实现（TASKS Phase 4 #3）")
    }

    fn is_available(&self) -> bool {
        // TODO(#3)：按 `AvailableRecognizerLanguages`/语言包支持度判断，当前恒 true。
        true
    }
}
