//! 构建脚本：Windows 目标嵌入 exe 图标资源（`assets/app.rc` → app.ico）。
//!
//! WSL2 交叉编译经 llvm-rc（AGENTS.md 5.1 工具链，`apt install llvm` 自带）；
//! 宿主 Linux 构建（cargo test）自动跳过。

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let _ = embed_resource::compile("assets/app.rc", embed_resource::NONE);
        println!("cargo:rerun-if-changed=assets/app.rc");
        println!("cargo:rerun-if-changed=assets/app.ico");
    }
}
