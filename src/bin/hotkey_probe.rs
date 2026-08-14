//! 热键诊断工具 v3（对照实验版，console 程序）。
//!
//! 外援结论：`Ctrl+Shift` 是 Windows 输入语言/键盘布局切换的默认热键，
//! 系统在按键到达 RegisterHotKey 分发**之前**抢跑，导致注册成功但
//! WM_HOTKEY 永不投递（中文系统尤其常见）。
//!
//! 本程序同时注册两组热键做对照：
//! - id=1：Ctrl+Shift+A（疑似被系统"输入语言切换"抢占）
//! - id=2：Ctrl+Alt+A（理论上不受影响）
//!
//! 请依次按下这两个组合键，观察窗口输出：
//! - 两个都收到 → 外援假设不成立，另行排查
//! - 只收到 id=2（Ctrl+Alt+A）→ 根因坐实：Ctrl+Shift 被系统输入法切换抢跑
//! - 两个都收不到 → 与 Ctrl+Shift 前缀无关，另有原因（驱动钩子等）

#[cfg(target_os = "windows")]
mod imp {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, TranslateMessage, MSG, WM_HOTKEY,
    };

    pub fn run() -> anyhow::Result<()> {
        println!("=== PrismaSnap hotkey probe v3 (对照实验) ===");

        unsafe {
            RegisterHotKey(None, 1, MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, u32::from(b'A'))?;
        }
        println!("[1] 已注册 Ctrl+Shift+A (id=1)  <- 疑似被系统输入法切换抢占");

        unsafe {
            RegisterHotKey(None, 2, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, u32::from(b'A'))?;
        }
        println!("[2] 已注册 Ctrl+Alt+A   (id=2)  <- 对照");

        println!("\n请依次按 Ctrl+Shift+A 和 Ctrl+Alt+A，观察下方输出（Ctrl+C 退出）...\n");

        let mut msg = MSG::default();
        loop {
            let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
            if ret.0 <= 0 {
                break;
            }
            if msg.message == WM_HOTKEY {
                let name = match msg.wParam.0 as i32 {
                    1 => "Ctrl+Shift+A",
                    2 => "Ctrl+Alt+A",
                    other => return Err(anyhow::anyhow!("未知热键 id: {other}")),
                };
                println!("收到 WM_HOTKEY: id={} ({name})", msg.wParam.0);
                println!(">>> 结论: {name} 链路正常");
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn main() -> anyhow::Result<()> {
    imp::run()
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("hotkey_probe only runs on Windows");
}
