//! 撤销/重做操作栈模块。
//!
//! 纯逻辑模块，跨平台兼容，配有单元测试。
//!
//! 栈模型：`done` 为已提交标注序列（绘制顺序 = 入栈顺序），`undone` 为
//! 被撤销的标注（栈顶是最近一次撤销）。新标注入栈时清空 `undone`——
//! 这是标准编辑器语义：撤销后做了新操作，重做分支即失效。

use super::Annotation;

/// 标注撤销/重做栈（快照模型：任意编辑均可撤销）。
///
/// 内部维护快照历史：`history` 栈顶即当前生效序列，`future` 为重做分支。
/// 单条 push / 原地改动 / 批量改动均以"整表快照"为粒度，语义清晰且
/// 与拖动等原地编辑兼容（旧实现仅支持 push/pop 会丢失 move 历史）。
#[derive(Debug, Default)]
pub struct UndoStack {
    /// 历史快照（栈顶 = 当前生效序列，底为 []）。
    history: Vec<Vec<Annotation>>,
    /// 重做分支（栈顶 = 最近一次被撤销的快照）。
    future: Vec<Vec<Annotation>>,
}

impl UndoStack {
    /// 创建空栈。
    pub fn new() -> Self {
        Self { history: vec![Vec::new()], future: Vec::new() }
    }

    fn current(&self) -> &Vec<Annotation> {
        self.history.last().expect("history 非空")
    }
    fn current_mut(&mut self) -> &mut Vec<Annotation> {
        self.history.last_mut().expect("history 非空")
    }

    /// 提交一条标注。提交后重做分支失效（清空 `future`）。
    pub fn push(&mut self, annotation: Annotation) {
        let mut next = self.current().clone();
        next.push(annotation);
        self.history.push(next);
        self.future.clear();
    }

    /// 原地替换指定下标的标注（拖动等编辑用），记录为一次可撤销编辑。
    /// 下标越界返回 false。
    pub fn replace(&mut self, index: usize, annotation: Annotation) -> bool {
        if index >= self.current().len() {
            return false;
        }
        let mut next = self.current().clone();
        next[index] = annotation;
        self.history.push(next);
        self.future.clear();
        true
    }

    /// 以闭包批量原地编辑当前序列（拖动提交等），记录为一次可撤销编辑。
    ///
    /// 闭包返回 true 表示确有改动，才压入历史；返回 false 视为无操作。
    pub fn edit_current(&mut self, f: impl FnOnce(&mut Vec<Annotation>) -> bool) -> bool {
        let mut next = self.current().clone();
        if !f(&mut next) {
            return false;
        }
        self.history.push(next);
        self.future.clear();
        true
    }

    /// 撤销最近一次提交，成功返回 `true`（栈空返回 `false`）。
    pub fn undo(&mut self) -> bool {
        if self.history.len() <= 1 {
            return false;
        }
        let cur = self.history.pop().expect("history 非空");
        self.future.push(cur);
        true
    }

    /// 重做最近一次撤销，成功返回 `true`（无可重做返回 `false`）。
    pub fn redo(&mut self) -> bool {
        match self.future.pop() {
            Some(snap) => {
                self.history.push(snap);
                true
            }
            None => false,
        }
    }

    /// 是否可撤销。
    pub fn can_undo(&self) -> bool {
        self.history.len() > 1
    }

    /// 是否可重做。
    pub fn can_redo(&self) -> bool {
        !self.future.is_empty()
    }

    /// 当前生效的标注序列（绘制顺序）。
    pub fn annotations(&self) -> &[Annotation] {
        self.current()
    }

    /// 当前生效序列的可变访问（注意：直接改动不会自动记录历史，
    /// 拖动等需通过 `replace`/`edit_current` 记录）。
    pub fn annotations_mut(&mut self) -> &mut Vec<Annotation> {
        self.current_mut()
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
