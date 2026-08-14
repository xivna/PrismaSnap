//! 全局热键注册与事件接收（仅 Windows 平台编译）。
//!
//! 基于 `global-hotkey`（底层 `RegisterHotKey`，不拦截无关按键，
//! 见 AGENTS.md 3.1 选型说明）。热键字符串格式兼容其 `FromStr`，
//! 如 `"Ctrl+Shift+A"`。事件经 `GlobalHotKeyEvent::receiver()` 的
//! channel 接收（跨线程 unbounded channel，无需消息循环）。

use anyhow::Context;
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

/// 热键管理器：持有注册关系，drop 不自动注销（进程退出即回收）。
///
/// 如需运行时改绑（设置界面改热键），用 [`HotkeyManager::rebind`]。
pub struct HotkeyManager {
    manager: GlobalHotKeyManager,
    hotkey: HotKey,
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
        Ok(Self { manager, hotkey })
    }

    /// 注销旧热键并注册新热键（设置界面改绑时调用）。
    ///
    /// * `new_str` - 新的热键字符串；解析失败或注册失败时旧热键保持有效。
    pub fn rebind(&mut self, new_str: &str) -> anyhow::Result<()> {
        let new_hotkey: HotKey = new_str
            .parse()
            .with_context(|| format!("解析热键字符串失败: {new_str}"))?;
        // 先注册新的，成功后再注销旧的，保证失败时旧热键不丢
        self.manager
            .register(new_hotkey)
            .with_context(|| format!("注册新热键失败（可能被其他程序占用）: {new_str}"))?;
        self.manager.unregister(self.hotkey)?;
        self.hotkey = new_hotkey;
        Ok(())
    }

    /// 当前已注册热键的 id（用于匹配事件）。
    pub fn id(&self) -> u32 {
        self.hotkey.id()
    }
}

/// 从全局热键事件 channel 取一个**按下**事件，且属于目标热键 id。
///
/// 返回 `true` 表示该事件已消费（截图触发）；无事件或无关事件返回 `false`。
/// 注意用 `try_recv` 非阻塞轮询，主循环自行控制节拍。
pub fn poll_trigger(hotkey_id: u32) -> bool {
    match GlobalHotKeyEvent::receiver().try_recv() {
        Ok(event) => event.state == HotKeyState::Pressed && event.id() == hotkey_id,
        Err(_) => false,
    }
}
