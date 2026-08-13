//! LLM 异步客户端模块。
//!
//! 基于 `reqwest`，兼容 OpenAI 格式 API（支持本地 llama.cpp 等）。
//! 异步任务通过 `tokio::spawn` 脱离 UI 线程，结果经 `EventLoopProxy` 回传（AGENTS.md 3.10 节）。
