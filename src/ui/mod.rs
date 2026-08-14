//! UI 模块。
//!
//! 包含托盘、覆盖层选区窗口、标注编辑器、工具条与设置主界面。
//! 覆盖层与编辑器采用单窗口切换架构（Snipaste 式），见 PROGRESS.md 决策记录。

pub mod editor;
pub mod overlay;
pub mod settings;
pub mod toolbar;

// 托盘基于 tray-icon（仅 Windows target 引入），WSL2 下不编译。
#[cfg(target_os = "windows")]
pub mod tray;
