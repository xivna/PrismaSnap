//! 屏幕捕获模块。
//!
//! 基于 `windows-capture`（Windows Graphics Capture API）。
//! 注意：`Capture::start()` 会接管调用线程，必须运行在独立线程，
//! 结果通过 channel 传回 UI 线程（见 AGENTS.md 3.1 节）。
//!
//! 模块划分：
//! - [`frame`]：帧数据与 Rgba16F → sRGB 转换（跨平台，WSL2 可单测）
//! - [`color`]：HDR 色彩转换纯函数（跨平台）
//! - [`engine`]：捕获引擎（仅 Windows）
//! - [`display_info`]：显示器参数查询（仅 Windows）

pub mod color;
pub mod frame;

#[cfg(target_os = "windows")]
pub mod display_info;
#[cfg(target_os = "windows")]
pub mod engine;
