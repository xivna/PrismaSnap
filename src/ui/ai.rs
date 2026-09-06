//! AI 后台任务（仅 Windows 平台编译）。
//!
//! 提取文字 / 翻译的耗时工作（OCR 同步推理、LLM 异步请求）跑在独立线程，
//! 完成经调用方注入的 `notify` 回调送回主线程（宿主将其接到 winit 的
//! `EventLoopProxy`，见 AGENTS.md 3.10 节）；宿主按 `req_id` 丢弃过期结果。
//!
//! OCR 返回的 bbox 为**选区图内本地坐标**，本模块按 `origin`（选区左上在
//! 全图中的位置）回偏为全图物理像素坐标，与 [`crate::ocr::TextBlock`] 等
//! 上下游的全图坐标约定对齐。

use image::DynamicImage;

use crate::config::{OcrEngineKind, TranslateConfig};
use crate::llm::client::{MultimodalBackend, TextBackend};
use crate::ocr::{BBox, TextBlock, TextRegion, TranslatedRegion};
use crate::translate::backend::TranslationBackend;
use crate::translate::{TranslatePipeline, WorkMode};

/// AI 任务完成回传（宿主按 `req_id` 丢弃过期结果）。
#[derive(Debug)]
pub enum AiDone {
    /// OCR 完成（`regions` 为全图坐标，失败时为展示用错误串）。
    Ocr {
        req_id: u64,
        regions: Result<Vec<TextRegion>, String>,
    },
    /// 翻译完成（`regions` 为待覆盖的译文区域，失败时为展示用错误串）。
    Translate {
        req_id: u64,
        regions: Result<Vec<TranslatedRegion>, String>,
    },
}

/// 起后台线程做 OCR 识别（同步推理，不占 tokio）。
///
/// * `image` - 选区裁剪图（sRGB）；
/// * `origin` - 选区左上在全图中的坐标，用于 bbox 回偏。
pub fn spawn_ocr_job(
    image: DynamicImage,
    origin: (i32, i32),
    kind: OcrEngineKind,
    req_id: u64,
    notify: impl Fn(AiDone) + Send + Sync + 'static,
) {
    std::thread::spawn(move || {
        let regions = run_ocr(&image, origin, kind).map_err(|e| format!("{e:#}"));
        notify(AiDone::Ocr { req_id, regions });
    });
}

/// 起后台线程做翻译（`current_thread` 运行时驱动异步管线，随任务销毁）。
///
/// * `image` - 选区裁剪图（采样背景/文字色与裁剪多模态小图用）；
/// * `blocks` - 全图坐标的语义块；
/// * `origin` - 选区左上在全图中的坐标（管线内裁剪/采样换算用）。
pub fn spawn_translate_job(
    image: DynamicImage,
    blocks: Vec<TextBlock>,
    origin: (i32, i32),
    cfg: TranslateConfig,
    req_id: u64,
    notify: impl Fn(AiDone) + Send + Sync + 'static,
) {
    std::thread::spawn(move || {
        let regions =
            run_translate(&image, blocks, origin, &cfg).map_err(|e| format!("{e:#}"));
        notify(AiDone::Translate { req_id, regions });
    });
}

/// OCR 同步执行体（引擎选择 + 可用性门控 + bbox 回偏）。
fn run_ocr(
    image: &DynamicImage,
    origin: (i32, i32),
    kind: OcrEngineKind,
) -> anyhow::Result<Vec<TextRegion>> {
    let engine = crate::ocr::create_engine(&kind);
    if !engine.is_available() {
        anyhow::bail!("OCR 引擎不可用（rapidocr 插件缺失时请在设置页切回自动/系统）");
    }
    let mut regions = engine.detect_and_recognize(image)?;
    for r in &mut regions {
        let (x, y) = (r.bbox.x, r.bbox.y);
        r.bbox = BBox {
            x: x.saturating_add_signed(origin.0),
            y: y.saturating_add_signed(origin.1),
            width: r.bbox.width,
            height: r.bbox.height,
        };
    }
    Ok(regions)
}

/// 翻译同步执行体（按配置组装双后端 + `WorkMode` 调度，见 AGENTS.md 3.8 节）。
fn run_translate(
    image: &DynamicImage,
    blocks: Vec<TextBlock>,
    origin: (i32, i32),
    cfg: &TranslateConfig,
) -> anyhow::Result<Vec<TranslatedRegion>> {
    let text = TextBackend::new(cfg.text_llm.clone(), cfg.prompts.clone())?;
    let prompts = cfg.prompts.clone();
    let multimodal: Option<Box<dyn TranslationBackend>> = cfg
        .effective_multimodal()
        .map(|ep| MultimodalBackend::new(ep, prompts))
        .transpose()?
        .map(|m| Box::new(m) as Box<dyn TranslationBackend>);
    let pipeline = TranslatePipeline {
        mode: WorkMode::from_config(cfg),
        target_lang: cfg.target_lang.clone(),
        text: Box::new(text),
        multimodal,
    };
    // 同步线程内自建 current_thread 运行时，随任务结束销毁，不常驻
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(pipeline.process(image, blocks, origin))
}
