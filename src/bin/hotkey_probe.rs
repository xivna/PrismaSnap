//! 热键诊断工具 v4（三组合对照实验，console 程序）。
//!
//! 背景：此前"Ctrl+Shift+A 不触发"的对照实验中，用户按键可能按错
//! （实际按了 Alt+Shift+A），导致"输入法抢跑"结论存疑，需重新严格验证。
//!
//! 本程序一次注册三个组合：
//! - id=1：Ctrl+Shift+A
//! - id=2：Ctrl+Alt+A
//! - id=3：Alt+Shift+A
//!
//! 请**严格**按提示依次按下这三个组合键，观察输出，每个组合重复 3 次。

#[cfg(target_os = "windows")]
mod imp {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, TranslateMessage, MSG, WM_HOTKEY,
    };

    pub fn run() -> anyhow::Result<()> {
        println!("=== PrismaSnap hotkey probe v4 (三组合对照) ===");

        unsafe {
            RegisterHotKey(None, 1, MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, u32::from(b'A'))?;
        }
        println!("[1] 已注册 Ctrl+Shift+A");
        unsafe {
            RegisterHotKey(None, 2, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, u32::from(b'A'))?;
        }
        println!("[2] 已注册 Ctrl+Alt+A");
        unsafe {
            RegisterHotKey(None, 3, MOD_ALT | MOD_SHIFT | MOD_NOREPEAT, u32::from(b'A'))?;
        }
        println!("[3] 已注册 Alt+Shift+A");

        println!("\n请严格按下组合键测试（每个组合建议按 3 次），按 Ctrl+C 退出：");
        println!("  第一步: 按住 Ctrl 和 Shift 不放,再按 A  → 期望输出 [1]");
        println!("  第二步: 按住 Ctrl 和 Alt 不放,再按 A   → 期望输出 [2]");
        println!("  第三步: 按住 Alt 和 Shift 不放,再按 A  → 期望输出 [3]\n");

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
                    3 => "Alt+Shift+A",
                    other => return Err(anyhow::anyhow!("未知热键 id: {other}")),
                };
                println!("收到 WM_HOTKEY: id={} -> {name} 链路正常", msg.wParam.0);
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
