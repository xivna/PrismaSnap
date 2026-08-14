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
    /// LLM API 配置（Phase 4 使用，先定义结构）。
    pub llm: LlmConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: String::from("Ctrl+Shift+A"),
            save: SaveConfig::default(),
            capture: CaptureConfig::default(),
            llm: LlmConfig::default(),
        }
    }
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
    /// 色彩映射降级开关：HDR 数据不做归一化增益，直接 clamp 当 SDR 处理，
    /// 用于规避特定显卡驱动在 HDR 模式下的已知色差问题（AGENTS.md 3.2 节）。
    pub hdr_degrade: bool,
}

/// LLM API 配置（OpenAI 兼容格式，支持本地 llama.cpp 等）。
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

impl Config {
    /// 默认配置文件路径：可执行文件同目录下的 `config.toml`（便携模式）。
    pub fn default_path() -> anyhow::Result<PathBuf> {
        Ok(paths::exe_dir()?.join("config.toml"))
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
        let config: Config = toml::from_str(&text)?;
        Ok(config)
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
}
