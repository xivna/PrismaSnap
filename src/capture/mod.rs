//! 屏幕捕获模块。
//!
//! 基于 `windows-capture`（Windows Graphics Capture API）。
//! 注意：`Capture::start()` 会接管调用线程，必须运行在独立线程，
//! 结果通过 channel 传回 UI 线程（见 AGENTS.md 3.1 节）。

pub mod color;

#[cfg(target_os = "windows")]
pub mod display_info;
