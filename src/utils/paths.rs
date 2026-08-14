//! 便携式路径辅助（跨平台）。
//!
//! 所有文件读写必须基于可执行文件所在目录或用户显式指定的路径，
//! 禁止硬编码绝对路径（AGENTS.md 5.2「便携版路径处理」）。

use std::path::PathBuf;

/// 可执行文件所在目录（便携式路径基准）。
///
/// # Errors
/// 无法获取当前可执行文件路径时返回错误（极罕见，例如环境异常）。
pub fn exe_dir() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_dir_resolves_to_existing_dir() {
        let dir = exe_dir().unwrap();
        assert!(dir.is_dir());
    }
}
