//! UI 模块。
//!
//! 包含托盘、覆盖层选区窗口、标注编辑器、工具条与设置主界面。
//! 覆盖层与编辑器采用单窗口切换架构（Snipaste 式），见 PROGRESS.md 决策记录。

pub mod toolbar;

// AI 后台任务（OCR/翻译线程与回传类型，仅 Windows，见 ai.rs）。
#[cfg(target_os = "windows")]
pub mod ai;

// 编辑器画布依赖 egui（仅 Windows target 引入），WSL2 下不编译。
#[cfg(target_os = "windows")]
pub mod editor;

// 覆盖层 / 设置界面 / wgpu 渲染栈 / 托盘基于 Windows 平台依赖，WSL2 下不编译。
#[cfg(target_os = "windows")]
pub mod gui;
#[cfg(target_os = "windows")]
pub mod overlay;
#[cfg(target_os = "windows")]
pub mod settings;
#[cfg(target_os = "windows")]
pub mod tray;
