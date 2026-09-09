//! 配置加载与保存（跨平台，TOML 格式）。
//!
//! 路径策略（AGENTS.md 3.5 节，便携优先）：
//! 1. 优先查找可执行文件同目录下的 `config.toml`；
//! 2. 若不存在，则创建默认配置文件。
//!
//! 所有字段都有 serde 默认值，旧版配置文件缺失新字段也能正常加载。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::utils::paths;

/// 应用总配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 全局截图热键字符串，格式如 `"Ctrl+Shift+A"`（global-hotkey 的 `FromStr` 格式）。
    pub hotkey: String,
    /// 保存行为。
    pub save: SaveConfig,
    /// 捕获选项。
    pub capture: CaptureConfig,
    /// 界面外观。
    pub ui: UiConfig,
    /// LLM API 配置（旧 `[llm]` 段，仅为向后兼容保留读取；新配置走 `translate.text_llm`）。
    pub llm: LlmConfig,
    /// OCR 引擎配置（见 AGENTS.md 3.8 节）。
    pub ocr: OcrConfig,
    /// 翻译管线配置（见 AGENTS.md 3.8 节）。
    pub translate: TranslateConfig,
    /// 日志配置（级别改后重启生效）。
    pub logging: LoggingConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // 注意：不要用 Ctrl+Shift 开头的组合——那是 Windows 中文系统
            // "输入语言切换"的默认热键，会在按键到达 RegisterHotKey 之前
            // 被系统抢跑（注册成功但永不触发，见 docs/热键问题排查.md）
            hotkey: String::from("Ctrl+Alt+A"),
            save: SaveConfig::default(),
            capture: CaptureConfig::default(),
            ui: UiConfig::default(),
            llm: LlmConfig::default(),
            ocr: OcrConfig::default(),
            translate: TranslateConfig::default(),
            logging: LoggingConfig::default(),
        }
    }
}

/// 日志级别（设置页通用区，改后重启生效；TOML 里小写，如 `level = "debug"`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// 只记错误。
    Error,
    /// 错误 + 警告。
    Warn,
    /// 默认：错误 + 警告 + 关键流程。
    #[default]
    Info,
    /// 再加 LLM 原始回包、管线分支等诊断细节（出问题定位用）。
    Debug,
    /// 最啰嗦（含框架底层）。
    Trace,
}

impl LogLevel {
    /// 转 tracing 过滤字符串（`RUST_LOG` 环境变量仍优先，见 `logging::init`）。
    pub fn as_filter(&self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

/// 日志配置。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// 日志级别（默认 `info`）。
    pub level: LogLevel,
}

/// 界面外观配置。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// 主题（深色工具条文字辨识度差，默认浅色）。
    pub theme: Theme,
    /// 界面字体文件路径（设置页下拉选择；空 = 系统默认微软雅黑链路）。
    pub interface_font: String,
    /// 标注/翻译字体文件路径（文字标注 + 译文覆盖渲染及对应预览；空 = 系统默认）。
    pub annotation_font: String,
}

/// 界面主题（TOML 里蛇形小写，如 `theme = "light"`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    /// 浅色（默认）。
    #[default]
    Light,
    /// 深色。
    Dark,
}

/// 保存行为配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SaveConfig {
    /// 保存模式：`silent` 静默保存至 `dir`；`always_ask` 每次询问位置
    /// （Phase 2 无对话框阶段暂时退化为静默保存）。
    pub mode: SaveMode,
    /// 静默保存目录（相对路径按可执行文件目录解析；空则用 exe 下 `screenshots/`）。
    pub dir: PathBuf,
    /// 保存格式。
    pub format: SaveFormat,
    /// JPEG 质量 1~100（仅 JPEG 格式生效）。
    pub jpeg_quality: u8,
}

impl Default for SaveConfig {
    fn default() -> Self {
        Self {
            mode: SaveMode::Silent,
            dir: PathBuf::new(),
            format: SaveFormat::Png,
            jpeg_quality: 90,
        }
    }
}

/// 保存模式（TOML 里蛇形小写字符串，如 `mode = "always_ask"`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveMode {
    /// 每次询问保存位置。
    AlwaysAsk,
    /// 静默保存到指定目录。
    Silent,
}

/// 保存格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveFormat {
    /// PNG（无损，默认）。
    Png,
    /// JPEG（有损，体积小）。
    Jpeg,
}

/// 捕获选项。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureConfig {
    /// 截图是否包含系统光标（默认不含，见 AGENTS.md 3.1「光标捕获需显式配置」）。
    pub cursor_visible: bool,
}

/// LLM API 配置（OpenAI 兼容格式，支持本地 llama.cpp 等）。
///
/// 旧 `[llm]` 段结构，仅为向后兼容保留：`Config::load` 会在新字段仍为默认值时
/// 把这里的值迁移到 `translate.text_llm` / `translate.target_lang`（见
/// [`Config::migrate_legacy_llm`]）。新代码一律读写 `TranslateConfig`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    /// API 完整地址（含 /v1/chat/completions 路径）。
    pub api_url: String,
    /// API Key（本地服务可留空）。
    pub api_key: String,
    /// 模型名。
    pub model: String,
    /// 翻译目标语言。
    pub translate_target: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            api_url: String::from("http://127.0.0.1:8080/v1/chat/completions"),
            api_key: String::new(),
            model: String::new(),
            translate_target: String::from("简体中文"),
        }
    }
}

/// OCR 引擎选择（AGENTS.md 3.8 节；TOML 里蛇形小写，如 `engine = "rapidocr"`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OcrEngineKind {
    /// 自动：`RapidOcrEngine` 可用（`plugins/ocr/` 模型齐全）则用它，否则退回系统 OCR。
    #[default]
    Auto,
    /// 强制使用系统 OCR（`Windows.Media.Ocr`，内置兜底）。
    System,
    /// 强制使用 RapidOCR 插件；模型缺失时 OCR 不可用（调用方按降级链提示）。
    Rapidocr,
}

/// OCR 引擎配置。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OcrConfig {
    /// 引擎选择（默认 `auto`）。
    pub engine: OcrEngineKind,
}

/// 翻译工作模式（设置菜单三选一，默认 `auto`，见 AGENTS.md 3.8 节）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslateMode {
    /// 按 OCR 置信度自动分流（默认）：高置信走纯文本，低置信走裁剪多模态。
    #[default]
    Auto,
    /// 区域裁剪 → 多模态 LLM（识别 + 翻译一体）。
    CropMultimodal,
    /// OCR 识别 → 纯文本 LLM 翻译。
    OcrText,
}

/// 多模态 LLM 配置模式（设置页"多模态LLM"卡片三选一，默认与基础相同）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MultimodalMode {
    /// 与基础 LLM 相同（默认）：多模态请求复用 `text_llm` 的 URL/Key/Model。
    #[default]
    SameAsText,
    /// 不配置多模态大模型：模式一不可用，Auto 退化为纯模式二。
    Disabled,
    /// 自定义：使用本结构体内独立的 URL/Key/Model。
    Custom,
}

/// 单个 LLM 后端接入点（OpenAI 兼容，URL 含 `/v1/chat/completions` 路径）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmEndpoint {
    /// API 完整地址。
    pub api_url: String,
    /// API Key（本地服务可留空）。
    pub api_key: String,
    /// 模型名。
    pub model: String,
}

impl Default for LlmEndpoint {
    fn default() -> Self {
        Self {
            api_url: String::from("http://127.0.0.1:8080/v1/chat/completions"),
            api_key: String::new(),
            model: String::new(),
        }
    }
}

/// 多模态 LLM 配置（设置页新增卡片，基础卡片保持不动，见 AGENTS.md 3.8 节）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MultimodalLlmConfig {
    /// 三选一模式（默认 `same_as_text`）。
    pub mode: MultimodalMode,
    /// 自定义 API 地址（仅 `mode = custom` 时生效）。
    pub api_url: String,
    /// 自定义 API Key（仅 `mode = custom` 时生效）。
    pub api_key: String,
    /// 自定义模型名（仅 `mode = custom` 时生效）。
    pub model: String,
}

impl Default for MultimodalLlmConfig {
    fn default() -> Self {
        Self {
            mode: MultimodalMode::SameAsText,
            api_url: String::new(),
            api_key: String::new(),
            model: String::new(),
        }
    }
}

/// 默认翻译目标语言。
pub const DEFAULT_TARGET_LANG: &str = "简体中文";

/// 默认 Auto 模式置信度阈值（低于此值的区域走裁剪多模态路径）。
pub const DEFAULT_CONFIDENCE_THRESHOLD: f32 = 0.85;

/// 翻译管线配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TranslateConfig {
    /// 工作模式（默认 `auto`）。
    pub mode: TranslateMode,
    /// 翻译目标语言（默认简体中文）。
    pub target_lang: String,
    /// Auto 模式置信度阈值（0.5~0.95，默认 0.85）。
    pub confidence_threshold: f32,
    /// 文本翻译后端（= 设置页基础 LLM 卡片）。
    pub text_llm: LlmEndpoint,
    /// 多模态后端（= 设置页新增多模态卡片）。
    pub multimodal_llm: MultimodalLlmConfig,
    /// 提示词模板（= 设置页提示词卡片；留空即用内置默认）。
    pub prompts: TranslatePrompts,
}

impl Default for TranslateConfig {
    fn default() -> Self {
        Self {
            mode: TranslateMode::Auto,
            target_lang: String::from(DEFAULT_TARGET_LANG),
            confidence_threshold: DEFAULT_CONFIDENCE_THRESHOLD,
            text_llm: LlmEndpoint::default(),
            multimodal_llm: MultimodalLlmConfig::default(),
            prompts: TranslatePrompts::default(),
        }
    }
}

/// LLM 提示词模板（设置页可自定义；任一条留空即用内置默认）。
///
/// 占位符：`{target}` = 目标语言；`{items}` = 输入 JSON 数组（仅纯文本模板）；
/// `{count}` = 图片数量（仅多模态批量模板）。未知占位符原样保留。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TranslatePrompts {
    /// 纯文本翻译模板（含 `{target}`、`{items}`）。
    pub text: String,
    /// 多模态单图模板（含 `{target}`）。
    pub multimodal_single: String,
    /// 多模态批量模板（含 `{target}`、`{count}`）。
    pub multimodal_batch: String,
}

/// 内置纯文本提示词模板（与此前硬编码一致，含占位符）。
pub const DEFAULT_TEXT_PROMPT: &str = "你是专业翻译。请将下面 JSON 数组中每一项的 \"text\" 字段翻译为{target}，\n\
并保持 \"id\" 不变、条目数量不变、顺序不变，不要合并或拆分条目。\n\
严格按以下 JSON 格式输出，不要输出任何解释性文字：\n\
[{\"id\": 1, \"translation\": \"...\"}, {\"id\": 2, \"translation\": \"...\"}]\n\
\n\
输入：\n\
[{items}]";
/// 内置多模态单图提示词模板。
pub const DEFAULT_MULTIMODAL_SINGLE_PROMPT: &str = "你是专业的图像文字识别与翻译助手。这是一张截图局部区域的图片，\n\
其中包含一行或多行文字。请完成：\n\
1. 按阅读顺序识别图片中的所有文字；\n\
2. 将识别结果翻译为{target}；\n\
3. 严格按以下 JSON 格式输出，不要输出任何多余内容：\n\
{\"original\": \"识别出的原文\", \"translation\": \"对应译文\"}\n\
若图片中没有可识别的文字，输出 {\"original\": \"\", \"translation\": \"\"}";
/// 内置多模态批量提示词模板。
pub const DEFAULT_MULTIMODAL_BATCH_PROMPT: &str = "你是专业的图像文字识别与翻译助手。下面有 {count} 张截图局部区域的图片，\n\
每张包含一行或多行文字。请对每张图完成：\n\
1. 按阅读顺序识别图片中的所有文字；\n\
2. 将识别结果翻译为{target}；\n\
3. 严格按输入图片顺序返回一个 JSON 数组，每项带序号，不要输出任何多余内容：\n\
[{\"index\": 0, \"original\": \"原文\", \"translation\": \"译文\"}, ...]\n\
若某张图没有可识别的文字，该项输出 {\"index\": N, \"original\": \"\", \"translation\": \"\"}";

impl Default for TranslatePrompts {
    fn default() -> Self {
        Self {
            text: String::from(DEFAULT_TEXT_PROMPT),
            multimodal_single: String::from(DEFAULT_MULTIMODAL_SINGLE_PROMPT),
            multimodal_batch: String::from(DEFAULT_MULTIMODAL_BATCH_PROMPT),
        }
    }
}

impl TranslatePrompts {
    /// 取生效模板（空字符串回退内置默认，保证总有可用提示词）。
    pub fn effective_text(&self) -> &str {
        if self.text.trim().is_empty() {
            DEFAULT_TEXT_PROMPT
        } else {
            &self.text
        }
    }

    /// 取生效模板（空字符串回退内置默认）。
    pub fn effective_multimodal_single(&self) -> &str {
        if self.multimodal_single.trim().is_empty() {
            DEFAULT_MULTIMODAL_SINGLE_PROMPT
        } else {
            &self.multimodal_single
        }
    }

    /// 取生效模板（空字符串回退内置默认）。
    pub fn effective_multimodal_batch(&self) -> &str {
        if self.multimodal_batch.trim().is_empty() {
            DEFAULT_MULTIMODAL_BATCH_PROMPT
        } else {
            &self.multimodal_batch
        }
    }
}

impl TranslateConfig {
    /// 当前生效的多模态后端配置。
    ///
    /// - `SameAsText`（默认）：复用 `text_llm`；
    /// - `Custom`：使用 `multimodal_llm` 自带的 URL/Key/Model；
    /// - `Disabled`：返回 `None`，调用方应禁用模式一、Auto 按纯模式二跑。
    pub fn effective_multimodal(&self) -> Option<LlmEndpoint> {
        match self.multimodal_llm.mode {
            MultimodalMode::SameAsText => Some(self.text_llm.clone()),
            MultimodalMode::Custom => Some(LlmEndpoint {
                api_url: self.multimodal_llm.api_url.clone(),
                api_key: self.multimodal_llm.api_key.clone(),
                model: self.multimodal_llm.model.clone(),
            }),
            MultimodalMode::Disabled => None,
        }
    }
}

impl Config {
    /// 默认配置文件路径：可执行文件同目录下的 `config.toml`（便携模式）。
    pub fn default_path() -> anyhow::Result<PathBuf> {
        Ok(paths::exe_dir()?.join("config.toml"))
    }

    /// 只读日志级别（日志系统初始化之前调用；文件缺失/损坏一律回默认，
    /// 不报错——此时日志还没建，报错也没处记）。
    pub fn load_log_level(path: &Path) -> String {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| toml::from_str::<Config>(&text).ok())
            .map(|c| c.logging.level.as_filter().to_string())
            .unwrap_or_else(|| String::from("info"))
    }

    /// 从指定路径加载配置。
    ///
    /// 文件不存在时创建默认配置并保存，返回默认值；
    /// 文件存在但解析失败时返回错误（不静默覆盖用户的坏配置）。
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            let config = Config::default();
            config.save(path)?;
            return Ok(config);
        }
        let text = std::fs::read_to_string(path)?;
        let mut config: Config = toml::from_str(&text)?;
        config.migrate_legacy_llm();
        Ok(config)
    }

    /// 把旧 `[llm]` 段迁移到新 `[translate]` 结构（向后兼容）。
    ///
    /// 仅当新字段仍为默认值时才迁移（新配置优先，老用户配置不丢失）；
    /// 全新默认文件（新旧皆默认）走个过场，不改变任何值。
    fn migrate_legacy_llm(&mut self) {
        if self.llm == LlmConfig::default() {
            return;
        }
        let legacy_endpoint = LlmEndpoint {
            api_url: self.llm.api_url.clone(),
            api_key: self.llm.api_key.clone(),
            model: self.llm.model.clone(),
        };
        if self.translate.text_llm == LlmEndpoint::default()
            && legacy_endpoint != LlmEndpoint::default()
        {
            self.translate.text_llm = legacy_endpoint;
        }
        if self.translate.target_lang == DEFAULT_TARGET_LANG
            && self.llm.translate_target != DEFAULT_TARGET_LANG
        {
            self.translate.target_lang = self.llm.translate_target.clone();
        }
    }

    /// 把配置序列化为 TOML 写入指定路径（原子写：先写临时文件再改名）。
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("prismsnap_config_{name}.toml"))
    }

    #[test]
    fn default_roundtrip() {
        let path = temp_config_path("roundtrip");
        let config = Config::default();
        config.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(config, loaded);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_creates_default() {
        let path = temp_config_path("missing");
        let _ = std::fs::remove_file(&path);
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded, Config::default());
        assert!(path.exists(), "应自动创建默认配置文件");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn partial_toml_fills_defaults() {
        let path = temp_config_path("partial");
        std::fs::write(
            &path,
            r#"
hotkey = "Alt+F1"

[save]
mode = "always_ask"

[llm]
model = "gpt-4o"
"#,
        )
        .unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.hotkey, "Alt+F1");
        assert_eq!(loaded.save.mode, SaveMode::AlwaysAsk);
        // 未写的字段取默认值
        assert_eq!(loaded.save.format, SaveFormat::Png);
        assert_eq!(loaded.llm.model, "gpt-4o");
        assert!(!loaded.capture.cursor_visible);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn invalid_toml_returns_error() {
        let path = temp_config_path("invalid");
        std::fs::write(&path, "hotkey = [broken").unwrap();
        assert!(Config::load(&path).is_err(), "坏配置应报错而非静默覆盖");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_is_atomic_tmp_removed() {
        let path = temp_config_path("atomic");
        Config::default().save(&path).unwrap();
        assert!(!path.with_extension("toml.tmp").exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn translate_defaults_match_spec() {
        let t = TranslateConfig::default();
        assert_eq!(t.mode, TranslateMode::Auto);
        assert_eq!(t.target_lang, "简体中文");
        assert!((t.confidence_threshold - 0.85).abs() < f32::EPSILON);
        assert_eq!(t.multimodal_llm.mode, MultimodalMode::SameAsText);
        assert_eq!(Config::default().ocr.engine, OcrEngineKind::Auto);
    }

    #[test]
    fn multimodal_same_as_text_reuses_text_llm() {
        let mut config = Config::default();
        config.translate.text_llm.model = String::from("gpt-4o");
        let effective = config
            .translate
            .effective_multimodal()
            .expect("默认应复用基础配置");
        assert_eq!(effective.model, "gpt-4o");
        assert_eq!(effective.api_url, config.translate.text_llm.api_url);
    }

    #[test]
    fn multimodal_disabled_returns_none() {
        let mut config = Config::default();
        config.translate.multimodal_llm.mode = MultimodalMode::Disabled;
        assert!(config.translate.effective_multimodal().is_none());
    }

    #[test]
    fn multimodal_custom_returns_custom_endpoint() {
        let mut config = Config::default();
        config.translate.multimodal_llm.mode = MultimodalMode::Custom;
        config.translate.multimodal_llm.model = String::from("qwen-vl-max");
        let effective = config
            .translate
            .effective_multimodal()
            .expect("自定义模式应返回独立配置");
        assert_eq!(effective.model, "qwen-vl-max");
    }

    #[test]
    fn legacy_llm_section_migrates_to_translate() {
        let path = temp_config_path("legacy_llm");
        std::fs::write(
            &path,
            r#"
[llm]
api_url = "http://192.168.1.10:8080/v1/chat/completions"
model = "qwen2.5"
translate_target = "English"
"#,
        )
        .unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(
            loaded.translate.text_llm.api_url,
            "http://192.168.1.10:8080/v1/chat/completions"
        );
        assert_eq!(loaded.translate.text_llm.model, "qwen2.5");
        assert_eq!(loaded.translate.target_lang, "English");
        // 多模态缺省即与基础相同
        assert_eq!(
            loaded.translate.multimodal_llm.mode,
            MultimodalMode::SameAsText
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn new_style_config_wins_over_legacy() {
        let path = temp_config_path("new_wins");
        std::fs::write(
            &path,
            r#"
[llm]
model = "old-model"

[translate.text_llm]
model = "new-model"
"#,
        )
        .unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.translate.text_llm.model, "new-model");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn prompts_default_and_empty_fallback() {
        let p = TranslatePrompts::default();
        assert!(p.effective_text().contains("{target}"));
        assert!(p.effective_text().contains("{items}"));
        assert!(p.effective_multimodal_single().contains("{target}"));
        assert!(p.effective_multimodal_batch().contains("{count}"));
        // 空模板回退默认
        let mut empty = TranslatePrompts {
            text: String::from("   "),
            multimodal_single: String::new(),
            multimodal_batch: String::new(),
        };
        assert_eq!(empty.effective_text(), DEFAULT_TEXT_PROMPT);
        assert_eq!(
            empty.effective_multimodal_single(),
            DEFAULT_MULTIMODAL_SINGLE_PROMPT
        );
        assert_eq!(
            empty.effective_multimodal_batch(),
            DEFAULT_MULTIMODAL_BATCH_PROMPT
        );
        // 非空原样返回
        empty.text = String::from("译{target}");
        assert_eq!(empty.effective_text(), "译{target}");
    }

    #[test]
    fn log_level_filter_strings() {
        assert_eq!(LogLevel::Info.as_filter(), "info");
        assert_eq!(LogLevel::Debug.as_filter(), "debug");
        assert_eq!(Config::default().logging.level, LogLevel::Info);
    }

    #[test]
    fn multimodal_mode_serde_roundtrip() {
        let path = temp_config_path("mm_mode");
        let mut config = Config::default();
        config.translate.multimodal_llm.mode = MultimodalMode::Disabled;
        config.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("disabled"), "TOML 应序列化为 snake_case: {text}");
        let loaded = Config::load(&path).unwrap();
        assert_eq!(
            loaded.translate.multimodal_llm.mode,
            MultimodalMode::Disabled
        );
        let _ = std::fs::remove_file(&path);
    }
}
