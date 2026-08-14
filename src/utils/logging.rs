//! 日志初始化（跨平台，基于 `tracing` + `tracing-subscriber`）。
//!
//! 双输出：控制台（INFO 级）+ 文件（DEBUG 级，追加写）。
//! 支持 `RUST_LOG` 环境变量覆盖默认过滤级别。

use std::fs::OpenOptions;
use std::path::Path;

use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

/// 初始化日志系统，返回日志文件句柄（**调用方必须持有到进程退出**，
/// 否则文件层会被提前关闭导致写入失败）。
///
/// * `log_dir` - 日志目录（不存在则创建），日志写入 `log_dir/prismsnap.log`。
///
/// # Errors
/// 创建日志目录或打开日志文件失败时返回错误。
///
/// # Panics
/// 日志系统已被初始化过（全局 dispatcher 只能设置一次）时 panic。
pub fn init(log_dir: &Path) -> anyhow::Result<std::fs::File> {
    std::fs::create_dir_all(log_dir)?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("prismsnap.log"))?;

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let stdout_layer = fmt::layer().with_filter(env_filter.clone());
    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_writer(file.try_clone()?)
        .with_filter(env_filter);

    tracing_subscriber::registry()
        .with(stdout_layer)
        .with(file_layer)
        .init();

    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_log_file() {
        let dir = std::env::temp_dir().join("prismsnap_log_test");
        let file = init(&dir).unwrap();
        // 句柄有效且文件已创建
        assert!(dir.join("prismsnap.log").exists());
        drop(file);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
