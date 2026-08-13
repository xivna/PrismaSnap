//! PrismaSnap 程序入口。
//!
//! 启动后默认无主窗口，仅显示系统托盘图标；通过全局快捷键触发截图。
//! 当前为骨架阶段，仅验证模块结构与交叉编译链路。

/// 程序入口。
fn main() -> anyhow::Result<()> {
    println!("PrismaSnap 启动成功（骨架验证）");
    if let Ok(exe) = std::env::current_exe() {
        println!("可执行文件路径：{}", exe.display());
    }
    Ok(())
}
