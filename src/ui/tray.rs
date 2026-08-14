//! 系统托盘（仅 Windows 平台编译）。
//!
//! 基于 `tray-icon` + `muda`。菜单事件通过 `MenuEvent::receiver()`
//! 的 channel 接收，注意 Windows 后端需要宿主消息循环（PeekMessage 泵），
//! 见 `main.rs` 的 `pump_messages`。
//!
//! 图标为程序内生成的简易棱镜图案（避免资源文件依赖），
//! 正式图标待 assets/icons 准备好后替换。

use anyhow::Context;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// 托盘菜单动作标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// 立即截图（同全局热键）。
    Capture,
    /// 退出程序。
    Exit,
}

/// 系统托盘封装。
pub struct Tray {
    /// 托盘图标句柄（保持存活，drop 即销毁图标）。
    _icon: TrayIcon,
    /// 「截图」菜单项 id。
    capture_id: MenuId,
    /// 「退出」菜单项 id。
    exit_id: MenuId,
}

impl Tray {
    /// 创建托盘图标与右键菜单。
    pub fn new() -> anyhow::Result<Self> {
        let menu = Menu::new();
        let capture_item = MenuItem::new("截图", true, None);
        let exit_item = MenuItem::new("退出", true, None);
        menu.append(&capture_item).context("添加「截图」菜单项失败")?;
        menu.append(&exit_item).context("添加「退出」菜单项失败")?;

        let capture_id = capture_item.id().clone();
        let exit_id = exit_item.id().clone();

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon(make_prism_icon()?)
            .with_tooltip("PrismaSnap")
            .build()
            .context("创建托盘图标失败")?;

        Ok(Self {
            _icon: icon,
            capture_id,
            exit_id,
        })
    }

    /// 非阻塞地取一个待处理的菜单动作（无事件时返回 `None`）。
    pub fn poll_action(&self) -> Option<TrayAction> {
        match MenuEvent::receiver().try_recv() {
            Ok(event) if event.id == self.capture_id => Some(TrayAction::Capture),
            Ok(event) if event.id == self.exit_id => Some(TrayAction::Exit),
            Ok(_) => None,
            Err(_) => None,
        }
    }
}

/// 程序内生成 32x32 棱镜图标：深蓝渐变底 + 白色三角。
///
/// 简单几何图形，避免外部资源文件；正式版本替换为 assets/icons 的图标。
fn make_prism_icon() -> anyhow::Result<Icon> {
    const SIZE: usize = 32;
    let mut rgba = vec![0u8; SIZE * SIZE * 4];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let i = (y * SIZE + x) * 4;
            // 深蓝渐变底
            let t = y as f32 / SIZE as f32;
            rgba[i] = (30.0 + 20.0 * t) as u8;
            rgba[i + 1] = (70.0 + 40.0 * t) as u8;
            rgba[i + 2] = (180.0 + 60.0 * t) as u8;
            rgba[i + 3] = 255;
            // 白色棱镜三角（顶点在上边中点，底边贴下边）
            let half_width = (y as f32 / SIZE as f32) * (SIZE as f32 / 2.0);
            let dx = (x as f32 - SIZE as f32 / 2.0).abs();
            if dx < half_width && y < SIZE - 3 {
                rgba[i] = 255;
                rgba[i + 1] = 255;
                rgba[i + 2] = 255;
            }
        }
    }
    Icon::from_rgba(rgba, SIZE as u32, SIZE as u32).context("生成托盘图标失败")
}
