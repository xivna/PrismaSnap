//! 通用辅助函数模块。

pub mod dpi;
pub mod image_codec;
pub mod logging;
pub mod math;
pub mod paths;
pub mod time;

// 剪贴板 / 单实例基于 Windows-only 依赖，WSL2 下不编译。
#[cfg(target_os = "windows")]
pub mod clipboard;
#[cfg(target_os = "windows")]
pub mod single_instance;
