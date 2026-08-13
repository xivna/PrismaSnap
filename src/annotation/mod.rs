//! 标注工具与渲染引擎模块。
//!
//! 导出工具 Trait、Manager 与渲染器；撤销/重做栈见 `undo_stack`。
//! 渲染一致性方案（GPU 读回 vs 全 CPU）待 Phase 3 落地前定夺（AGENTS.md 3.7 节）。

pub mod tools;
pub mod undo_stack;
