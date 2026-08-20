//! 撤销/重做操作栈模块。
//!
//! 纯逻辑模块，跨平台兼容，配有单元测试。
//!
//! 栈模型：`done` 为已提交标注序列（绘制顺序 = 入栈顺序），`undone` 为
//! 被撤销的标注（栈顶是最近一次撤销）。新标注入栈时清空 `undone`——
//! 这是标准编辑器语义：撤销后做了新操作，重做分支即失效。

use super::Annotation;

/// 标注撤销/重做栈。
#[derive(Debug, Default)]
pub struct UndoStack {
    /// 已提交的标注（绘制顺序）。
    done: Vec<Annotation>,
    /// 已撤销、可重做的标注（栈顶 = 最近一次撤销）。
    undone: Vec<Annotation>,
}

impl UndoStack {
    /// 创建空栈。
    pub fn new() -> Self {
        Self::default()
    }

    /// 提交一条标注。提交后重做分支失效（清空 `undone`）。
    pub fn push(&mut self, annotation: Annotation) {
        self.done.push(annotation);
        self.undone.clear();
    }

    /// 撤销最近一次提交，成功返回 `true`（栈空返回 `false`）。
    pub fn undo(&mut self) -> bool {
        match self.done.pop() {
            Some(a) => {
                self.undone.push(a);
                true
            }
            None => false,
        }
    }

    /// 重做最近一次撤销，成功返回 `true`（无可重做返回 `false`）。
    pub fn redo(&mut self) -> bool {
        match self.undone.pop() {
            Some(a) => {
                self.done.push(a);
                true
            }
            None => false,
        }
    }

    /// 是否可撤销。
    pub fn can_undo(&self) -> bool {
        !self.done.is_empty()
    }

    /// 是否可重做。
    pub fn can_redo(&self) -> bool {
        !self.undone.is_empty()
    }

    /// 当前生效的标注序列（绘制顺序）。
    pub fn annotations(&self) -> &[Annotation] {
        &self.done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::{Color, Tool};
    use crate::utils::math::Rect;

    /// 构造一条测试用矩形标注（坐标随意，互不相同的宽度便于区分）。
    fn rect_ann(width: u32) -> Annotation {
        Annotation::Rect {
            rect: Rect { x: 0, y: 0, width, height: 10 },
            color: Color::RED,
            stroke_width: 2.0,
        }
    }

    #[test]
    fn push_then_undo_redo_roundtrip() {
        let mut stack = UndoStack::new();
        assert!(!stack.can_undo() && !stack.can_redo());

        stack.push(rect_ann(1));
        stack.push(rect_ann(2));
        assert_eq!(stack.annotations().len(), 2);
        assert!(stack.can_undo() && !stack.can_redo());

        assert!(stack.undo());
        assert_eq!(stack.annotations().len(), 1);
        assert!(stack.can_redo());

        assert!(stack.redo());
        assert_eq!(stack.annotations().len(), 2);
        assert!(!stack.can_redo());
    }

    #[test]
    fn undo_on_empty_returns_false() {
        let mut stack = UndoStack::new();
        assert!(!stack.undo());
        assert!(!stack.redo());
    }

    #[test]
    fn new_push_clears_redo_branch() {
        let mut stack = UndoStack::new();
        stack.push(rect_ann(1));
        assert!(stack.undo());
        assert!(stack.can_redo());

        // 撤销后提交新标注 → 重做分支失效
        stack.push(rect_ann(3));
        assert!(!stack.can_redo());
        assert_eq!(stack.annotations().len(), 1);
    }

    #[test]
    fn undo_order_is_lifo() {
        let mut stack = UndoStack::new();
        stack.push(rect_ann(1));
        stack.push(rect_ann(2));
        stack.undo();
        // 撤销的是最后提交的 width=2
        match &stack.annotations()[0] {
            Annotation::Rect { rect, .. } => assert_eq!(rect.width, 1),
            _ => panic!("应为矩形标注"),
        }
    }

    /// 确保 `Tool` 与栈语义无关（栈只管标注序列，不关心工具类型）。
    #[test]
    fn stack_is_tool_agnostic() {
        let mut stack = UndoStack::new();
        stack.push(rect_ann(1));
        assert_eq!(stack.annotations().len(), 1);
        let _ = Tool::ALL; // 工具枚举存在性检查
    }
}
