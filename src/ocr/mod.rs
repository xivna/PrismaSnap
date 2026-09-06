//! OCR 引擎层（可插拔，见 AGENTS.md 3.8 节）。
//!
//! - 核心类型（[`BBox`] / [`TextRegion`] / [`TextBlock`] / [`TranslatedRegion`]）
//!   为跨平台纯数据，区域合并/翻译调度/渲染等纯逻辑模块直接复用，可在 WSL2 下单测；
//! - [`OcrEngine`] trait 统一检测接口：[`system::SystemOcrEngine`]（Windows-only，
//!   `Windows.Media.Ocr`，内置兜底）与 [`rapid::RapidOcrEngine`]（`plugins/ocr/`
//!   模型插件，高精度）分别实现，坐标永远由 OCR 提供、LLM 不参与定位；
//! - 工厂 [`create_engine`]（仅 Windows）按配置与插件探测结果选择引擎。

#[cfg(target_os = "windows")]
pub mod system;
pub mod rapid;
/// 插件模型下载（官方源自动下载 + 手动下载帮助，跨平台可单测）。
pub mod download;

pub use rapid::RapidOcrEngine;
#[cfg(target_os = "windows")]
pub use system::SystemOcrEngine;

use std::path::PathBuf;

/// OCR 文字区域包围盒（全图物理像素坐标，`x,y` 为左上角）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BBox {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl BBox {
    /// 面积（物理像素，`u64` 防大图溢出）。
    pub fn area(self) -> u64 {
        self.width as u64 * self.height as u64
    }

    /// 外接矩形（区域合并时求语义块包围盒用）。
    pub fn union(self, other: BBox) -> BBox {
        let x1 = self.x.min(other.x);
        let y1 = self.y.min(other.y);
        let x2 = self.x.saturating_add(self.width).max(other.x.saturating_add(other.width));
        let y2 = self.y.saturating_add(self.height).max(other.y.saturating_add(other.height));
        BBox { x: x1, y: y1, width: x2 - x1, height: y2 - y1 }
    }
}

/// OCR 检测/识别出的单个文字区域（通常是一行）。
#[derive(Debug, Clone)]
pub struct TextRegion {
    /// 区域编号（同一张图内唯一，便于 prompt id 对齐与调试）。
    pub id: usize,
    /// 包围盒（全图物理像素坐标）。
    pub bbox: BBox,
    /// 识别文字；`detect()` 仅检测阶段为 `None`（供模式一裁剪用）。
    pub text: Option<String>,
    /// 识别置信度（0.0~1.0，供 Auto 模式分流）。
    /// 注意：系统 OCR 不返回置信度，`SystemOcrEngine` 恒填 1.0。
    pub confidence: f32,
    /// 文本行倾斜角度（度，0 为水平；暂存，渲染层预留）。
    pub angle: f32,
    /// 由 bbox 高度反推的估算字号（物理像素，渲染初值）。
    pub est_font_size: u32,
}

impl TextRegion {
    /// 由 bbox 高度反推估算字号（全角字高约等于字号；拉丁行高略大属可接受误差，
    /// 渲染层会按译文宽度自适应缩小，见 AGENTS.md 3.8 节）。
    pub fn estimate_font_size(bbox_height: u32) -> u32 {
        bbox_height.max(8)
    }
}

/// 若干相邻 [`TextRegion`] 合并后的语义块（一句话/一段话，见 `translate::merge`）。
#[derive(Debug, Clone)]
pub struct TextBlock {
    /// 合并后的外接框（子区域 [`BBox::union`] 逐级求并）。
    pub bbox: BBox,
    /// 子区域（按阅读顺序排列）。
    pub regions: Vec<TextRegion>,
    /// 拼接后的文本；子区域含 `None`（仅检测模式）时为 `None`。
    pub merged_text: Option<String>,
}

impl TextBlock {
    /// 块置信度：子区域置信度平均值（Auto 模式分流依据；空块为 0.0）。
    pub fn confidence(&self) -> f32 {
        if self.regions.is_empty() {
            return 0.0;
        }
        let sum: f32 = self.regions.iter().map(|r| r.confidence).sum();
        sum / self.regions.len() as f32
    }

    /// 视觉行数：子区域按 y 分行计数（同行容差与检测排序一致，
    /// 见 `rapid::same_line_band` 思想：`max(10, 半行高)` 上限 40px）。
    ///
    /// 合并会把同行相邻框并进一个块（`merge::should_merge` 间距≈0 即合），
    /// 此时 `regions.len()` 是框数不是行数——断行必须按本函数，
    /// 否则单行原文会被当成多行拆散、字号也被多行高度压小
    /// （2026-09-06 实机：单行英文译后异常小）。
    pub fn visual_line_count(&self) -> usize {
        let mut sorted: Vec<&TextRegion> = self.regions.iter().collect();
        sorted.sort_by_key(|r| (r.bbox.y, r.bbox.x));
        let mut lines = 0usize;
        let mut first_y = 0u32;
        let mut band = 0u32;
        let mut first_in_band = true;
        for r in sorted {
            if first_in_band || r.bbox.y.saturating_sub(first_y) > band {
                lines += 1;
                first_y = r.bbox.y;
                band = (r.bbox.height / 2).clamp(10, 40);
                first_in_band = false;
            }
        }
        lines.max(1).min(self.regions.len().max(1))
    }
}

/// 词间拼接：CJK 相邻不加空格，其余加空格。
///
/// WinRT OCR 按词输出（无行文本），行内空格需按字符集还原；
/// RapidOCR 识别输出同样可经此归一化（跨平台纯函数，可单测）。
pub(crate) fn join_words(words: &[String]) -> String {
    let mut out = String::new();
    let mut prev_cjk = false;
    let mut first = true;
    for w in words {
        let cur_cjk = w.chars().next().is_some_and(is_cjk);
        if !first && !(prev_cjk && cur_cjk) {
            out.push(' ');
        }
        out.push_str(w);
        prev_cjk = w.chars().last().is_some_and(is_cjk);
        first = false;
    }
    out
}

/// 是否 CJK 字符（中日韩统一表意 + 假名 + 韩文 + 全角标点）。
fn is_cjk(ch: char) -> bool {
    matches!(ch,
        '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{20000}'..='\u{2A6DF}'
        | '\u{3040}'..='\u{309F}' | '\u{30A0}'..='\u{30FF}'
        | '\u{AC00}'..='\u{D7AF}' | '\u{1100}'..='\u{11FF}'
        | '\u{FF00}'..='\u{FFEF}')
}

/// 最终带译文的区域，供渲染层消费（背景擦除 + 译文覆盖，见 AGENTS.md 3.8 节）。
#[derive(Debug, Clone)]
pub struct TranslatedRegion {
    /// 原文字包围盒（全图物理像素坐标，译文覆盖于此之上）。
    pub bbox: BBox,
    /// 原文（模式一为多模态识别结果，模式二/Auto 高置信分支为 OCR 结果）。
    pub original: String,
    /// 译文（目标语言）。
    pub translated: String,
    /// 估算字号（物理像素，渲染初值，继承自原文字 bbox）。
    pub est_font_size: u32,
    /// 背景色（sRGB，bbox 边缘像素众数采样，用于擦除原文字）。
    pub bg_color: [u8; 3],
    /// 文字颜色（sRGB，原文字像素采样，尽量与原文视觉一致）。
    pub text_color: [u8; 3],
}

/// OCR 引擎抽象（`Send + Sync`，可在 `spawn_blocking` 里调用）。
///
/// 双实现：`SystemOcrEngine`（内置兜底）与 `RapidOcrEngine`（插件），上层经
/// [`create_engine`] 拿 `Box<dyn OcrEngine>`，无需感知具体引擎。
pub trait OcrEngine: Send + Sync {
    /// 引擎名（`"system"` / `"rapidocr"`，用于日志与设置页状态行）。
    fn name(&self) -> &'static str;

    /// 仅检测文字区域框，不识别内容——供模式一（裁剪→多模态）使用。
    fn detect(&self, image: &image::DynamicImage) -> anyhow::Result<Vec<TextRegion>>;

    /// 检测 + 识别一体，返回带文字内容的区域——供模式二/Auto 使用。
    fn detect_and_recognize(
        &self,
        image: &image::DynamicImage,
    ) -> anyhow::Result<Vec<TextRegion>>;

    /// 该引擎当前是否可用（`RapidOcrEngine` 取决于 `plugins/ocr/` 模型是否齐全）。
    fn is_available(&self) -> bool;
}

/// OCR 插件目录：可执行文件同目录 `plugins/ocr/`（便携包结构，见 AGENTS.md 3.6 节）。
///
/// # Errors
/// 无法获取可执行文件所在目录时返回错误（极罕见，例如环境异常）。
pub fn ocr_plugin_dir() -> anyhow::Result<PathBuf> {
    Ok(crate::utils::paths::exe_dir()?.join("plugins").join("ocr"))
}

/// 按配置与插件探测结果创建 OCR 引擎（仅 Windows）。
///
/// - `auto`：`RapidOcrEngine` 可用则用它，否则退回 `SystemOcrEngine`；
/// - `system` / `rapidocr`：强制指定引擎（调用方用前应检查 [`OcrEngine::is_available`]，
///   强制 rapid 但模型缺失时翻译按钮置灰并提示，不崩溃）。
#[cfg(target_os = "windows")]
pub fn create_engine(kind: &crate::config::OcrEngineKind) -> Box<dyn OcrEngine> {
    use crate::config::OcrEngineKind;

    let plugin_dir = ocr_plugin_dir().unwrap_or_else(|_| PathBuf::from("plugins/ocr"));
    let rapid = RapidOcrEngine::new(plugin_dir);
    match kind {
        OcrEngineKind::Rapidocr => Box::new(rapid),
        OcrEngineKind::System => Box::new(SystemOcrEngine),
        OcrEngineKind::Auto => {
            if rapid.is_available() {
                Box::new(rapid)
            } else {
                Box::new(SystemOcrEngine)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(id: usize, x: u32, y: u32, w: u32, h: u32, conf: f32) -> TextRegion {
        TextRegion {
            id,
            bbox: BBox { x, y, width: w, height: h },
            text: Some(format!("行{id}")),
            confidence: conf,
            angle: 0.0,
            est_font_size: TextRegion::estimate_font_size(h),
        }
    }

    #[test]
    fn bbox_union_covers_both() {
        let a = BBox { x: 10, y: 20, width: 30, height: 10 };
        let b = BBox { x: 25, y: 15, width: 20, height: 30 };
        assert_eq!(
            a.union(b),
            BBox { x: 10, y: 15, width: 35, height: 30 }
        );
        assert_eq!(a.area(), 300);
    }

    #[test]
    fn block_confidence_is_mean() {
        let block = TextBlock {
            bbox: BBox { x: 0, y: 0, width: 100, height: 40 },
            regions: vec![region(0, 0, 0, 100, 20, 0.9), region(1, 0, 22, 100, 18, 0.7)],
            merged_text: Some(String::from("行0行1")),
        };
        assert!((block.confidence() - 0.8).abs() < 1e-6);
    }

    #[test]
    fn visual_lines_count_bands_not_boxes() {
        // 同行两框（y 差 3）→ 1 行；下方再来一行（y 差 30）→ 2 行
        let block = TextBlock {
            bbox: BBox { x: 0, y: 0, width: 200, height: 60 },
            regions: vec![
                region(0, 0, 0, 90, 16, 0.9),
                region(1, 100, 3, 90, 16, 0.9),
                region(2, 0, 35, 200, 16, 0.9),
            ],
            merged_text: Some(String::from("ab\nc")),
        };
        assert_eq!(block.visual_line_count(), 2);
    }

    #[test]
    fn visual_lines_single_box_is_one() {
        let block = TextBlock {
            bbox: BBox { x: 0, y: 0, width: 100, height: 20 },
            regions: vec![region(0, 0, 0, 100, 20, 0.9)],
            merged_text: Some(String::from("Hi")),
        };
        assert_eq!(block.visual_line_count(), 1);
    }

    #[test]
    fn empty_block_confidence_is_zero() {
        let block = TextBlock {
            bbox: BBox { x: 0, y: 0, width: 10, height: 10 },
            regions: vec![],
            merged_text: None,
        };
        assert_eq!(block.confidence(), 0.0);
    }

    #[test]
    fn font_size_floor_is_min_readable() {
        assert_eq!(TextRegion::estimate_font_size(24), 24);
        assert_eq!(TextRegion::estimate_font_size(3), 8);
    }

    #[test]
    fn plugin_dir_is_under_exe_dir() {
        let dir = ocr_plugin_dir().unwrap();
        assert!(dir.ends_with(std::path::Path::new("plugins/ocr")));
    }

    #[test]
    fn cjk_joins_without_space() {
        assert_eq!(join_words(&["你好".into(), "世界".into()]), "你好世界");
    }

    #[test]
    fn latin_joins_with_space() {
        assert_eq!(join_words(&["Hello".into(), "world".into()]), "Hello world");
    }

    #[test]
    fn mixed_boundary_gets_space() {
        // "中文" + "ABC"：边界中→拉丁，加空格
        assert_eq!(join_words(&["中文".into(), "ABC".into()]), "中文 ABC");
        assert_eq!(join_words(&["ABC".into(), "中文".into()]), "ABC 中文");
    }

    /// trait 对象安全与线程安全约束的编译期断言（含 mock 实现）。
    #[test]
    fn engine_trait_is_object_safe() {
        struct MockEngine;
        impl OcrEngine for MockEngine {
            fn name(&self) -> &'static str {
                "mock"
            }
            fn detect(&self, _image: &image::DynamicImage) -> anyhow::Result<Vec<TextRegion>> {
                Ok(vec![region(0, 1, 2, 30, 16, 1.0)])
            }
            fn detect_and_recognize(
                &self,
                image: &image::DynamicImage,
            ) -> anyhow::Result<Vec<TextRegion>> {
                self.detect(image)
            }
            fn is_available(&self) -> bool {
                true
            }
        }

        fn assert_send_sync<T: Send + Sync>(_: &T) {}
        let engine: Box<dyn OcrEngine> = Box::new(MockEngine);
        assert_send_sync(&engine);
        assert_eq!(engine.name(), "mock");
        let img = image::DynamicImage::new_rgb8(64, 32);
        assert_eq!(engine.detect(&img).unwrap().len(), 1);
    }
}
