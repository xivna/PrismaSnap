//! 翻译管线（跨平台纯逻辑，见 AGENTS.md 3.8 节）。
//!
//! - [`merge`]：行 → 语义块版面分析；
//! - [`backend`]：双后端 Prompt 模板 + 宽松 JSON 解析 + [`backend::TranslationBackend`]；
//! - [`render`]：背景擦除 + 字号自适应渲染；
//! - [`WorkMode`]：三模式定义与配置映射（设置菜单三选一，默认 Auto）；
//! - [`TranslatePipeline::process`]：模式调度编排（Auto 双分支 `tokio::join!`
//!   并发 + 降级链：多模态失败回退文本兜底，否则该区域跳过不覆盖）。
//!
//! HTTP 组包与 `reqwest` 发送在 [`crate::llm::client`] 的双后端实现里，
//! 本模块只做编排与组装（纯逻辑，可单测；异步但与执行器无关）。

pub mod backend;
pub mod merge;
pub mod render;

use crate::config::{TranslateConfig, TranslateMode};
use crate::ocr::{BBox, TextBlock, TranslatedRegion};

/// 默认置信度阈值复用配置常量。
pub use crate::config::DEFAULT_CONFIDENCE_THRESHOLD;

/// 翻译工作模式（设置菜单三选一，**默认 Auto**，见 AGENTS.md 3.8 节）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WorkMode {
    /// 区域裁剪 → 多模态（识别 + 翻译一体）：艺术字/复杂背景更鲁棒，但贵。
    CropToMultimodal,
    /// OCR 识别 → 纯文本翻译：快、便宜、一次请求，适合标准字体。
    OcrThenTextLlm,
    /// 按块置信度自动分流（默认）：`>=` 阈值走纯文本，`<` 阈值走裁剪多模态。
    Auto {
        /// 置信度阈值（0.5~0.95，默认 0.85）。
        confidence_threshold: f32,
    },
}

impl Default for WorkMode {
    fn default() -> Self {
        Self::Auto { confidence_threshold: DEFAULT_CONFIDENCE_THRESHOLD }
    }
}

impl WorkMode {
    /// 阈值钳制下限（设置页滑块下限）。
    pub const MIN_THRESHOLD: f32 = 0.5;
    /// 阈值钳制上限（设置页滑块上限）。
    pub const MAX_THRESHOLD: f32 = 0.95;

    /// 由配置构造运行时模式（阈值钳制到滑块范围内）。
    pub fn from_config(config: &TranslateConfig) -> Self {
        match config.mode {
            TranslateMode::CropMultimodal => Self::CropToMultimodal,
            TranslateMode::OcrText => Self::OcrThenTextLlm,
            TranslateMode::Auto => Self::Auto {
                confidence_threshold: config
                    .confidence_threshold
                    .clamp(Self::MIN_THRESHOLD, Self::MAX_THRESHOLD),
            },
        }
    }

    /// Auto 模式是否启用（多模态未配置 `disabled` 时调用方应退化为纯模式二）。
    pub fn uses_multimodal(&self, multimodal_available: bool) -> bool {
        match self {
            Self::CropToMultimodal => multimodal_available,
            Self::OcrThenTextLlm => false,
            Self::Auto { .. } => multimodal_available,
        }
    }
}

/// 裁剪外扩（像素，给多模态留一点上下文；钳制在图内）。
pub const CROP_PAD: u32 = 4;

/// 翻译管线：模式调度 + 结果组装。
///
/// - `text`：纯文本后端（模式二，必配，即设置页基础 LLM）；
/// - `multimodal`：多模态后端（模式一；`None` 对应设置页"不配置"，
///   此时模式一不可用、Auto 退化为纯模式二）。
pub struct TranslatePipeline {
    pub mode: WorkMode,
    pub target_lang: String,
    pub text: Box<dyn backend::TranslationBackend>,
    pub multimodal: Option<Box<dyn backend::TranslationBackend>>,
}

impl TranslatePipeline {
    /// 端到端处理：语义块 → 按模式调度翻译 → 组装 `TranslatedRegion`。
    ///
    /// - `image`：待采样/裁剪的图（通常为选区图）；
    /// - `blocks`：全图物理像素坐标的语义块；
    /// - `origin`：`image` 相对全图的原点（选区图传选区左上，全图传 `(0, 0)`）。
    ///
    /// 降级（AGENTS.md 3.8）：多模态分支失败 → 有 OCR 文字则回退文本兜底，
    /// 否则该区域跳过不覆盖；纯文本整批失败 → 返回错误（调用方提示"部分区域失败"）。
    pub async fn process(
        &self,
        image: &image::DynamicImage,
        blocks: Vec<TextBlock>,
        origin: (i32, i32),
    ) -> anyhow::Result<Vec<TranslatedRegion>> {
        match self.mode {
            WorkMode::CropToMultimodal => {
                let mm = self.multimodal.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("未配置多模态模型，请先在设置页配置（与基础相同/自定义）")
                })?;
                self.translate_via_crop(image, &blocks, origin, mm).await
            }
            WorkMode::OcrThenTextLlm => self.translate_via_text(&blocks, image, origin).await,
            WorkMode::Auto { confidence_threshold } => {
                if !self.mode.uses_multimodal(self.multimodal.is_some()) {
                    // 多模态未配置：退化为纯模式二
                    return self.translate_via_text(&blocks, image, origin).await;
                }
                let mm = self
                    .multimodal
                    .as_ref()
                    .expect("uses_multimodal 已保证存在");
                let (high, low): (Vec<_>, Vec<_>) = blocks
                    .into_iter()
                    .partition(|b| b.confidence() >= confidence_threshold);
                let (high_r, low_r) = tokio::join!(
                    self.translate_via_text(&high, image, origin),
                    self.translate_via_crop(image, &low, origin, mm)
                );
                let mut out = high_r?;
                match low_r {
                    Ok(v) => out.extend(v),
                    Err(e) => {
                        tracing::warn!("多模态分支失败，回退文本兜底: {e:#}");
                        match self.translate_via_text(&low, image, origin).await {
                            Ok(v) => out.extend(v),
                            Err(e2) => {
                                tracing::warn!("文本兜底亦失败，跳过低置信区域: {e2:#}")
                            }
                        }
                    }
                }
                Ok(out)
            }
        }
    }

    /// 模式二：语义块文本一次打包翻译（空文本块跳过）。
    async fn translate_via_text(
        &self,
        blocks: &[TextBlock],
        image: &image::DynamicImage,
        origin: (i32, i32),
    ) -> anyhow::Result<Vec<TranslatedRegion>> {
        let idx: Vec<usize> = blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.merged_text.as_ref().is_some_and(|t| !t.trim().is_empty()))
            .map(|(i, _)| i)
            .collect();
        if idx.is_empty() {
            return Ok(vec![]);
        }
        let texts: Vec<String> = idx
            .iter()
            .map(|&i| blocks[i].merged_text.clone().unwrap_or_default())
            .collect();
        tracing::debug!("模式二：{} 块送纯文本翻译: {texts:?}", texts.len());
        let translated = self.text.translate_text(&texts, &self.target_lang).await?;
        let mut out = Vec::new();
        for (k, &i) in idx.iter().enumerate() {
            let t = translated.get(k).cloned().unwrap_or_default();
            if let Some(r) = assemble(image, &blocks[i], origin, t) {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// 模式一：按框裁剪 → 多模态识别 + 翻译一体（越界块跳过，空结果不覆盖）。
    async fn translate_via_crop(
        &self,
        image: &image::DynamicImage,
        blocks: &[TextBlock],
        origin: (i32, i32),
        mm: &Box<dyn backend::TranslationBackend>,
    ) -> anyhow::Result<Vec<TranslatedRegion>> {
        let mut kept: Vec<usize> = Vec::new();
        let mut crops: Vec<image::DynamicImage> = Vec::new();
        for (i, b) in blocks.iter().enumerate() {
            if let Some(crop) = crop_region(image, &b.bbox, origin) {
                kept.push(i);
                crops.push(crop);
            }
        }
        if crops.is_empty() {
            return Ok(vec![]);
        }
        tracing::debug!("模式一：{} 张裁剪图送多模态", crops.len());
        let pairs = mm.recognize_and_translate_image(&crops, &self.target_lang).await?;
        let mut out = Vec::new();
        for (k, &i) in kept.iter().enumerate() {
            let (original, translated) = pairs
                .get(k)
                .cloned()
                .unwrap_or((String::new(), String::new()));
            // 多模态识别出的原文优先；实在没有才用 OCR 文本（低置信仅供展示）。
            let original = if original.trim().is_empty() {
                blocks[i].merged_text.clone().unwrap_or_default()
            } else {
                original
            };
            if let Some(r) = assemble_with_original(image, &blocks[i], origin, original, translated)
            {
                out.push(r);
            }
        }
        Ok(out)
    }
}

/// 译文按原文行数断行（跨平台纯函数，可单测）。
///
/// LLM 常把多行原文译成单行字符串，直接渲染就是一条横贯的长行、与原文
/// 段落脱节。规则保守：**仅当译文为单行且原文有多行时**，按显示宽度贪心
/// 均分成同样行数；其余情况（译文自带换行/单行原文）原样返回。
/// 字符显示宽度按 CJK=1、其余=0.55 估算（精确贴合由下游 fit+wrap 保证）。
pub fn match_source_lines(translated: &str, line_count: usize) -> String {
    if line_count <= 1 || translated.contains('\n') {
        return translated.to_string();
    }
    let chars: Vec<char> = translated.chars().collect();
    let n = chars.len();
    if n == 0 {
        return String::new();
    }
    let total: f32 = chars.iter().map(|&c| char_units(c)).sum();
    let per_line = total / line_count as f32;
    let mut boundaries: Vec<usize> = Vec::new();
    let mut acc = 0.0;
    for (i, &ch) in chars.iter().enumerate() {
        acc += char_units(ch);
        // 当前行达配额、行数未用完、且剩余字够填满剩余行（每行至少 1 字）才断
        // （`acc` 断行后清零，故配额为相对当前行起点的 `per_line`）
        if boundaries.len() + 1 < line_count
            && acc >= per_line
            && n - (i + 1) >= line_count - boundaries.len() - 1
        {
            boundaries.push(i + 1);
            acc = 0.0;
        }
    }
    let mut lines: Vec<String> = Vec::with_capacity(line_count);
    let mut start = 0;
    for b in boundaries {
        lines.push(chars[start..b].iter().collect());
        start = b;
    }
    lines.push(chars[start..].iter().collect());
    lines.join("\n")
}

/// 字符显示宽度单位（CJK 全角=1，其余半角≈0.55），断行估算用。
fn char_units(ch: char) -> f32 {
    if matches!(ch,
        '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{20000}'..='\u{2A6DF}'
        | '\u{3040}'..='\u{309F}' | '\u{30A0}'..='\u{30FF}'
        | '\u{AC00}'..='\u{D7AF}' | '\u{1100}'..='\u{11FF}'
        | '\u{FF00}'..='\u{FFEF}')
    {
        1.0
    } else {
        0.55
    }
}

/// 按全图 bbox（含外扩）在图内裁剪（`origin` 为图相对全图的原点）；完全越界返回 `None`。
pub fn crop_region(
    image: &image::DynamicImage,
    bbox: &BBox,
    origin: (i32, i32),
) -> Option<image::DynamicImage> {
    let (w, h) = (image.width() as i32, image.height() as i32);
    let pad = CROP_PAD as i32;
    let x0 = (bbox.x as i32 - origin.0 - pad).clamp(0, w);
    let y0 = (bbox.y as i32 - origin.1 - pad).clamp(0, h);
    let x1 = (bbox.x as i32 - origin.0 + bbox.width as i32 + pad).clamp(0, w);
    let y1 = (bbox.y as i32 - origin.1 + bbox.height as i32 + pad).clamp(0, h);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(image.crop_imm(x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32))
}

/// 组装译文区域（原文取块文本；空译文返回 `None` 表示不覆盖）。
fn assemble(
    image: &image::DynamicImage,
    block: &TextBlock,
    origin: (i32, i32),
    translated: String,
) -> Option<TranslatedRegion> {
    let original = block.merged_text.clone().unwrap_or_default();
    assemble_with_original(image, block, origin, original, translated)
}

/// 组装译文区域（原文显式指定，供多模态分支用识别原文）。
fn assemble_with_original(
    image: &image::DynamicImage,
    block: &TextBlock,
    origin: (i32, i32),
    original: String,
    translated: String,
) -> Option<TranslatedRegion> {
    if translated.trim().is_empty() {
        return None;
    }
    // 单行译文按原文视觉行数断行（多行原文被 LLM 压成一行时恢复段落结构；
    // 注意用视觉行数而非框数：同行多框只算一行，否则单行会被硬拆、字号也被压小）
    let translated = match_source_lines(&translated, block.visual_line_count());
    // 图内本地框（采样/裁剪用），越界钳制由采样函数处理
    let local = BBox {
        x: (block.bbox.x as i32 - origin.0).max(0) as u32,
        y: (block.bbox.y as i32 - origin.1).max(0) as u32,
        width: block.bbox.width,
        height: block.bbox.height,
    };
    let bg = render::sample_bg_color(image, local);
    let fg = render::sample_text_color(image, local, bg);
    // 字号取子区域最大估算（多行块不用外接总高反推，会虚大，渲染层再按宽度自适应）
    let est = block.regions.iter().map(|r| r.est_font_size).max().unwrap_or(8);
    Some(TranslatedRegion {
        bbox: block.bbox,
        original,
        translated,
        est_font_size: est,
        bg_color: bg,
        text_color: fg,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::TextRegion;

    #[test]
    fn default_is_auto_085() {
        assert_eq!(
            WorkMode::default(),
            WorkMode::Auto { confidence_threshold: 0.85 }
        );
    }

    #[test]
    fn from_config_maps_modes() {
        let mut config = TranslateConfig::default();
        assert!(matches!(WorkMode::from_config(&config), WorkMode::Auto { .. }));
        config.mode = TranslateMode::CropMultimodal;
        assert_eq!(WorkMode::from_config(&config), WorkMode::CropToMultimodal);
        config.mode = TranslateMode::OcrText;
        assert_eq!(WorkMode::from_config(&config), WorkMode::OcrThenTextLlm);
    }

    #[test]
    fn threshold_is_clamped() {
        let mut config = TranslateConfig::default();
        config.confidence_threshold = 0.1;
        assert_eq!(
            WorkMode::from_config(&config),
            WorkMode::Auto { confidence_threshold: 0.5 }
        );
        config.confidence_threshold = 0.99;
        assert_eq!(
            WorkMode::from_config(&config),
            WorkMode::Auto { confidence_threshold: 0.95 }
        );
    }

    #[test]
    fn multimodal_gating() {
        assert!(WorkMode::CropToMultimodal.uses_multimodal(true));
        assert!(!WorkMode::CropToMultimodal.uses_multimodal(false));
        assert!(!WorkMode::OcrThenTextLlm.uses_multimodal(true));
        assert!(!WorkMode::default().uses_multimodal(false));
    }

    /// 管线单测共用：白底 fixture 与单行块构造（各用例自带轻量 mock 后端）。

    fn fixture_image() -> image::DynamicImage {
        image::DynamicImage::new_rgb8(200, 100)
    }

    fn block(id: usize, y: u32, conf: f32, text: &str) -> TextBlock {
        let bbox = BBox { x: 10, y, width: 100, height: 20 };
        TextBlock {
            bbox,
            regions: vec![TextRegion {
                id,
                bbox,
                text: Some(text.to_owned()),
                confidence: conf,
                angle: 0.0,
                est_font_size: 20,
            }],
            merged_text: Some(text.to_owned()),
        }
    }

    #[tokio::test]
    async fn auto_splits_high_text_low_crop() {
        let text_calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let mm_calls = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        struct T {
            calls: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>,
        }
        struct M {
            calls: std::sync::Arc<std::sync::Mutex<usize>>,
        }
        #[async_trait::async_trait]
        impl backend::TranslationBackend for T {
            async fn translate_text(&self, texts: &[String], _t: &str) -> anyhow::Result<Vec<String>> {
                self.calls.lock().unwrap().push(texts.to_vec());
                Ok(texts.iter().map(|t| format!("T:{t}")).collect())
            }
            async fn recognize_and_translate_image(&self, _c: &[image::DynamicImage], _t: &str) -> anyhow::Result<Vec<(String, String)>> {
                anyhow::bail!("nope")
            }
        }
        #[async_trait::async_trait]
        impl backend::TranslationBackend for M {
            async fn translate_text(&self, _t: &[String], _t2: &str) -> anyhow::Result<Vec<String>> {
                anyhow::bail!("nope")
            }
            async fn recognize_and_translate_image(&self, crops: &[image::DynamicImage], _t: &str) -> anyhow::Result<Vec<(String, String)>> {
                *self.calls.lock().unwrap() += 1;
                Ok((0..crops.len()).map(|i| (format!("O{i}"), format!("M{i}"))).collect())
            }
        }
        let pipe = TranslatePipeline {
            mode: WorkMode::Auto { confidence_threshold: 0.85 },
            target_lang: String::from("简体中文"),
            text: Box::new(T { calls: text_calls.clone() }),
            multimodal: Some(Box::new(M { calls: mm_calls.clone() })),
        };
        let img = fixture_image();
        let out = pipe
            .process(&img, vec![block(0, 10, 0.95, "Hello"), block(1, 40, 0.5, "xxx")], (0, 0))
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
        // 高置信走文本
        assert_eq!(out[0].translated, "T:Hello");
        assert_eq!(out[0].bbox.y, 10);
        // 低置信走裁剪（原文为多模态识别结果）
        assert_eq!(out[1].translated, "M0");
        assert_eq!(out[1].original, "O0");
        assert_eq!(text_calls.lock().unwrap().as_slice(), &[vec![String::from("Hello")]]);
        assert_eq!(*mm_calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn auto_without_multimodal_runs_all_text() {
        struct T;
        #[async_trait::async_trait]
        impl backend::TranslationBackend for T {
            async fn translate_text(&self, texts: &[String], _t: &str) -> anyhow::Result<Vec<String>> {
                Ok(texts.iter().map(|t| format!("T:{t}")).collect())
            }
            async fn recognize_and_translate_image(&self, _c: &[image::DynamicImage], _t: &str) -> anyhow::Result<Vec<(String, String)>> {
                anyhow::bail!("nope")
            }
        }
        let pipe = TranslatePipeline {
            mode: WorkMode::default(),
            target_lang: String::from("简体中文"),
            text: Box::new(T),
            multimodal: None,
        };
        let out = pipe
            .process(&fixture_image(), vec![block(0, 10, 0.95, "A"), block(1, 40, 0.1, "B")], (0, 0))
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|r| r.translated.starts_with("T:")));
    }

    #[tokio::test]
    async fn crop_without_multimodal_errors() {
        struct T;
        #[async_trait::async_trait]
        impl backend::TranslationBackend for T {
            async fn translate_text(&self, texts: &[String], _t: &str) -> anyhow::Result<Vec<String>> {
                Ok(texts.to_vec())
            }
            async fn recognize_and_translate_image(&self, _c: &[image::DynamicImage], _t: &str) -> anyhow::Result<Vec<(String, String)>> {
                anyhow::bail!("nope")
            }
        }
        let pipe = TranslatePipeline {
            mode: WorkMode::CropToMultimodal,
            target_lang: String::from("简体中文"),
            text: Box::new(T),
            multimodal: None,
        };
        let err = pipe.process(&fixture_image(), vec![block(0, 10, 0.9, "A")], (0, 0)).await.unwrap_err();
        assert!(err.to_string().contains("多模态"), "{err:#}");
    }

    #[tokio::test]
    async fn multimodal_failure_falls_back_to_text() {
        struct T;
        struct M;
        #[async_trait::async_trait]
        impl backend::TranslationBackend for T {
            async fn translate_text(&self, texts: &[String], _t: &str) -> anyhow::Result<Vec<String>> {
                Ok(texts.iter().map(|t| format!("T:{t}")).collect())
            }
            async fn recognize_and_translate_image(&self, _c: &[image::DynamicImage], _t: &str) -> anyhow::Result<Vec<(String, String)>> {
                anyhow::bail!("nope")
            }
        }
        #[async_trait::async_trait]
        impl backend::TranslationBackend for M {
            async fn translate_text(&self, _t: &[String], _t2: &str) -> anyhow::Result<Vec<String>> {
                anyhow::bail!("nope")
            }
            async fn recognize_and_translate_image(&self, _c: &[image::DynamicImage], _t: &str) -> anyhow::Result<Vec<(String, String)>> {
                anyhow::bail!("mock mm boom")
            }
        }
        let pipe = TranslatePipeline {
            mode: WorkMode::Auto { confidence_threshold: 0.85 },
            target_lang: String::from("简体中文"),
            text: Box::new(T),
            multimodal: Some(Box::new(M)),
        };
        let out = pipe
            .process(&fixture_image(), vec![block(0, 10, 0.2, "low")], (0, 0))
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].translated, "T:low");
    }

    #[tokio::test]
    async fn empty_translation_is_skipped() {
        struct T;
        #[async_trait::async_trait]
        impl backend::TranslationBackend for T {
            async fn translate_text(&self, texts: &[String], _t: &str) -> anyhow::Result<Vec<String>> {
                Ok(vec![String::new(); texts.len()])
            }
            async fn recognize_and_translate_image(&self, _c: &[image::DynamicImage], _t: &str) -> anyhow::Result<Vec<(String, String)>> {
                anyhow::bail!("nope")
            }
        }
        let pipe = TranslatePipeline {
            mode: WorkMode::OcrThenTextLlm,
            target_lang: String::from("简体中文"),
            text: Box::new(T),
            multimodal: None,
        };
        let out = pipe
            .process(&fixture_image(), vec![block(0, 10, 0.9, "A")], (0, 0))
            .await
            .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn single_line_splits_to_source_line_count() {
        // 3 行原文被压成单行 → 恢复 3 行，不丢字
        let out = match_source_lines("按年龄和新加坡各行业薪资二十五岁", 3);
        assert_eq!(out.lines().count(), 3, "{out:?}");
        assert_eq!(out.replace('\n', ""), "按年龄和新加坡各行业薪资二十五岁");
    }

    #[test]
    fn multiline_translation_is_respected() {
        assert_eq!(match_source_lines("甲\n乙", 3), "甲\n乙");
        assert_eq!(match_source_lines("单行", 1), "单行");
        assert_eq!(match_source_lines("", 3), "");
    }

    #[test]
    fn ascii_splits_evenly() {
        assert_eq!(match_source_lines("abcdefgh", 2), "abcd\nefgh");
    }

    #[test]
    fn crop_region_clamps_and_rejects_oob() {        let img = fixture_image();
        // 部分越界 → 钳制后 Some（含 4px 外扩）
        let c = crop_region(&img, &BBox { x: 150, y: 80, width: 100, height: 50 }, (0, 0)).unwrap();
        assert_eq!((c.width(), c.height()), (54, 24));
        // 完全越界 → None
        assert!(crop_region(&img, &BBox { x: 500, y: 500, width: 10, height: 10 }, (0, 0)).is_none());
        // origin 平移
        let c2 = crop_region(&img, &BBox { x: 60, y: 30, width: 20, height: 10 }, (50, 20)).unwrap();
        assert_eq!((c2.width(), c2.height()), (28, 18)); // 20+8pad × 10+8pad
    }
}
