//! 全局热键注册（仅 Windows 平台编译）。
//!
//! 基于 `global-hotkey`（底层 `RegisterHotKey`，不拦截无关按键，
//! 见 AGENTS.md 3.1 选型说明）。热键字符串格式兼容其 `FromStr`，
//! 如 `"Ctrl+Shift+A"`。
//!
//! **触发检测走 WM_HOTKEY 窗口消息**（在 `main.rs` 的消息泵里），
//! 不走 global-hotkey 的事件 channel：RegisterHotKey 把消息投递到
//! 注册窗口所在线程的消息队列，channel 方案需要多一层窗口过程回调，
//! 消息级检测更直接可靠（见 PROGRESS.md 已知问题记录）。

use anyhow::Context;
use global_hotkey::hotkey::HotKey;
use global_hotkey::GlobalHotKeyManager;

/// 热键管理器：持有注册关系，drop 不自动注销（进程退出即回收）。
///
/// 如需运行时改绑（设置界面录制热键），用 [`HotkeyManager::rebind`]。
/// 录制期间用 [`HotkeyManager::suspend`] 临时注销，避免旧热键拦截用户
/// 正在录制的组合键（重录相同组合时 WM_HOTKEY 会先于窗口按键被系统拦截）。
pub struct HotkeyManager {
    manager: GlobalHotKeyManager,
    hotkey: HotKey,
    /// 当前热键是否已注册（`suspend` 后为 false）。
    registered: bool,
}

impl HotkeyManager {
    /// 解析热键字符串并注册到系统。
    ///
    /// * `hotkey_str` - 如 `"Ctrl+Shift+A"`（格式同 global-hotkey 的 `FromStr`）。
    pub fn register(hotkey_str: &str) -> anyhow::Result<Self> {
        let hotkey: HotKey = hotkey_str
            .parse()
            .with_context(|| format!("解析热键字符串失败: {hotkey_str}"))?;
        let manager = GlobalHotKeyManager::new().context("创建全局热键管理器失败")?;
        manager
            .register(hotkey)
            .with_context(|| format!("注册热键失败（可能被其他程序占用）: {hotkey_str}"))?;
        Ok(Self {
            manager,
            hotkey,
            registered: true,
        })
    }

    /// 注销旧热键并注册新热键（设置界面录制新热键后调用）。
    ///
    /// * `new_str` - 新的热键字符串；解析失败或注册失败时旧热键保持有效。
    pub fn rebind(&mut self, new_str: &str) -> anyhow::Result<()> {
        let new_hotkey: HotKey = new_str
            .parse()
            .with_context(|| format!("解析热键字符串失败: {new_str}"))?;
        // 与当前热键相同且已注册：无需操作
        if self.registered && new_hotkey == self.hotkey {
            return Ok(());
        }
        // 先注册新的，成功后再注销旧的，保证失败时旧热键不丢
        self.manager
            .register(new_hotkey)
            .with_context(|| format!("注册新热键失败（可能被其他程序占用）: {new_str}"))?;
        if self.registered {
            self.manager
                .unregister(self.hotkey)
                .with_context(|| format!("注销旧热键失败: {}", self.hotkey))?;
        }
        self.hotkey = new_hotkey;
        self.registered = true;
        Ok(())
    }

    /// 临时注销热键（录制期间调用，幂等）。
    pub fn suspend(&mut self) -> anyhow::Result<()> {
        if self.registered {
            self.manager.unregister(self.hotkey)?;
            self.registered = false;
        }
        Ok(())
    }

    /// 恢复注册热键（录制结束调用，幂等）。
    pub fn resume(&mut self) -> anyhow::Result<()> {
        if !self.registered {
            self.manager.register(self.hotkey)?;
            self.registered = true;
        }
        Ok(())
    }

    /// 当前已注册热键的 id（WM_HOTKEY 消息的 wParam 与之比较）。
    pub fn id(&self) -> u32 {
        self.hotkey.id()
    }
}
