//! 通用辅助函数模块。

// 剪贴板基于 arboard（仅 Windows target 引入），WSL2 下不编译。
#[cfg(target_os = "windows")]
pub mod clipboard;
pub mod dpi;
pub mod image_codec;
pub mod math;
pub mod paths;
