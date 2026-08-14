//! PrismaSnap 库入口。
//!
//! 屏幕捕获、标注、LLM、UI 等各模块在此统一导出，供 bin（`main.rs`、
//! `hdr_probe.rs`）与 `tests/` 集成测试复用。

pub mod annotation;
pub mod capture;
pub mod config;
pub mod llm;
pub mod ui;
pub mod utils;

// 热键基于 global-hotkey（仅 Windows target 引入），WSL2 下不编译。
#[cfg(target_os = "windows")]
pub mod hotkey;
