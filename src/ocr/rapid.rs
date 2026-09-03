//! RapidOCR 插件引擎（PP-OCR ONNX 模型，`plugins/ocr/` 即放即用，见 AGENTS.md 3.8 节）。
//!
//! 本文件当前仅做**插件探测**（模型文件是否齐全，跨平台纯逻辑，可单测）；
//! 真正的 `ort` 推理（`det.onnx` 检测 + `rec.onnx` 识别）在 TASKS Phase 4 #4 落地，
//! 在此之前 `detect` 系列方法返回明确错误（不静默造假数据）。
//!
//! 插件规则：`>5M` 的模型文件 + `onnxruntime.dll` 全部放在程序目录
//! `plugins/ocr/` 下，不入版本控制；缺文件时 [`OcrEngine::is_available`] 为 false，
//! 工厂自动退回系统 OCR（见 [`crate::ocr::create_engine`]）。

use std::path::{Path, PathBuf};

use super::{OcrEngine, TextRegion};

/// 文本检测模型文件名（`>5M`，插件目录提供）。
pub const DET_MODEL_FILE: &str = "det.onnx";
/// 文本识别模型文件名（`>5M`，插件目录提供）。
pub const REC_MODEL_FILE: &str = "rec.onnx";
/// 识别字典文件名。
pub const KEYS_FILE: &str = "keys.txt";
/// ONNX Runtime 动态库文件名（`ort` 仅 `load-dynamic`，运行时从插件目录加载）。
#[cfg(target_os = "windows")]
pub const RUNTIME_LIB_FILE: &str = "onnxruntime.dll";
/// 非 Windows 平台的动态库文件名（探测逻辑跨平台单测用）。
#[cfg(target_os = "linux")]
pub const RUNTIME_LIB_FILE: &str = "libonnxruntime.so";
/// 非 Windows 平台的动态库文件名（探测逻辑跨平台单测用）。
#[cfg(target_os = "macos")]
pub const RUNTIME_LIB_FILE: &str = "libonnxruntime.dylib";

/// RapidOCR 插件引擎（`ort` + PP-OCR，`dir` 指向 `plugins/ocr/`）。
#[derive(Debug, Clone)]
pub struct RapidOcrEngine {
    dir: PathBuf,
}

impl RapidOcrEngine {
    /// 用插件目录构造（目录可不存在，此时 [`OcrEngine::is_available`] 为 false）。
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// 当前指向的插件目录。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 插件文件是否齐全（三个模型文件 + 运行时动态库均为普通文件）。
    pub fn model_files_present(dir: &Path) -> bool {
        [DET_MODEL_FILE, REC_MODEL_FILE, KEYS_FILE, RUNTIME_LIB_FILE]
            .iter()
            .all(|f| dir.join(f).is_file())
    }
}

impl OcrEngine for RapidOcrEngine {
    fn name(&self) -> &'static str {
        "rapidocr"
    }

    fn detect(&self, _image: &image::DynamicImage) -> anyhow::Result<Vec<TextRegion>> {
        anyhow::bail!("RapidOCR 推理尚未实现（TASKS Phase 4 #4）：请先安装插件或用系统 OCR")
    }

    fn detect_and_recognize(
        &self,
        _image: &image::DynamicImage,
    ) -> anyhow::Result<Vec<TextRegion>> {
        anyhow::bail!("RapidOCR 推理尚未实现（TASKS Phase 4 #4）：请先安装插件或用系统 OCR")
    }

    fn is_available(&self) -> bool {
        Self::model_files_present(&self.dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_plugin_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("prismsnap_ocr_{name}"))
    }

    fn clean(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_dir_is_unavailable() {
        let dir = temp_plugin_dir("missing");
        clean(&dir);
        assert!(!RapidOcrEngine::model_files_present(&dir));
        assert!(!RapidOcrEngine::new(dir).is_available());
    }

    #[test]
    fn complete_plugin_dir_is_available() {
        let dir = temp_plugin_dir("complete");
        clean(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in [DET_MODEL_FILE, REC_MODEL_FILE, KEYS_FILE, RUNTIME_LIB_FILE] {
            std::fs::write(dir.join(f), b"dummy").unwrap();
        }
        assert!(RapidOcrEngine::new(dir.clone()).is_available());
        clean(&dir);
    }

    #[test]
    fn partial_plugin_dir_is_unavailable() {
        let dir = temp_plugin_dir("partial");
        clean(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 缺 onnxruntime 动态库即视为不可用（运行时加载不到会崩，不如提前降级）
        for f in [DET_MODEL_FILE, REC_MODEL_FILE, KEYS_FILE] {
            std::fs::write(dir.join(f), b"dummy").unwrap();
        }
        assert!(!RapidOcrEngine::new(dir.clone()).is_available());
        clean(&dir);
    }
}
