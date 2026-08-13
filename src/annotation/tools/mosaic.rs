//! 马赛克 / 模糊标注工具。
//!
//! 注意：像素化无现成函数，需自行实现（分块降采样再放大）；
//! 模糊可用 `imageproc` 的 `gaussian_blur_f32`/`box_filter`（AGENTS.md 3.7 节）。
