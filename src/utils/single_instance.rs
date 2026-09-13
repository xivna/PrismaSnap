//! 单实例检测（仅 Windows 平台编译）。
//!
//! 用具名 Mutex（`CreateMutexW` + `ERROR_ALREADY_EXISTS`）实现，
//! 对应 AGENTS.md 风险清单第 10 条：防止重复启动导致多个托盘图标共存。
//! 不依赖 crates.io 上已停止维护的 `single-instance` crate。

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::Owned;

/// 单实例守卫：持有具名 Mutex 句柄，drop 时自动释放。
///
/// 进程存活期间 Mutex 保持占用；进程退出时系统自动回收，无需显式释放。
pub struct SingleInstance {
    _mutex: Owned<HANDLE>,
}

/// 尝试获取单实例锁。
///
/// * `name` - 互斥锁名（全局命名空间，建议含程序名，如 `"PrismaSnap"`）。
///
/// 返回 `Ok(Some(guard))` 表示本进程是唯一实例；
/// 返回 `Ok(None)` 表示已有其他实例在运行；
/// 返回 `Err` 仅表示系统调用失败（与"已有实例"区分开）。
pub fn acquire(name: &str) -> anyhow::Result<Option<SingleInstance>> {
    let wide_name: HSTRING = HSTRING::from(name);
    let handle = unsafe { CreateMutexW(None, true, PCWSTR(wide_name.as_ptr())) }?;
    // CreateMutexW 成功但句柄为空视为失败
    if handle.is_invalid() {
        return Err(windows::core::Error::from_thread().into());
    }
    let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already_exists {
        // 已有实例：关闭刚打开的句柄，返回 None
        unsafe { let _ = CloseHandle(handle); }
        return Ok(None);
    }
    Ok(Some(SingleInstance {
        _mutex: unsafe { Owned::new(handle) },
    }))
}
