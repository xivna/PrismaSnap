//! 大模型集成模块。
//!
//! 包含 `reqwest` 异步客户端与 API 调用、翻译/提取结果的叠加渲染。
//! 文字定位采用降级方案（固定位置横幅/气泡），定位层设计为可替换。

pub mod client;
pub mod overlay_render;
