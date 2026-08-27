//! 屏幕捕获引擎（仅 Windows 平台编译）。
//!
//! 基于 `windows-capture`（Windows Graphics Capture API）。关键约束（AGENTS.md 3.1）：
//! - `Capture::start()` 会**接管调用它的线程**直到捕获结束，因此捕获必须跑在
//!   独立线程，结果通过 channel 传回调用方。
//! - 光标捕获显式配置为 `WithoutCursor`（截图工具自己控制光标呈现，不用系统合成）。
//! - 颜色格式固定 `Rgba16F`（scRGB 线性 f16），HDR 转换在跨平台的
//!   [`super::frame`] 里完成。

use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context as _;

use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DrawBorderSettings, DirtyRegionSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use super::frame::RawFrame;
use crate::utils::math::Rect;

/// 等待首帧的最长时限。
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);
/// `recv_timeout` 的单次轮询间隔（期间可检查捕获线程是否已异常退出）。
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 捕获线程与 `capture_frame` 之间的通道负载。
type FrameResult = Result<RawFrame, String>;

/// 注入捕获句柄的上下文（经 `Settings` 的 flags 参数传入 `new()`，
/// 绕开 `GraphicsCaptureApiHandler::start` 无法注入实例的限制）。
struct CaptureContext {
    /// 结果回传通道（帧数据或错误信息）。
    tx: Sender<FrameResult>,
    /// 来源显示器的 GDI 设备名（如 `\\.\DISPLAY1`），供后续查询 SDR 白点。
    device_name: String,
}

/// 捕获句柄：收到首帧后提取数据、停止捕获、经 channel 回传。
struct FrameExtractor {
    tx: Sender<FrameResult>,
    device_name: String,
}

impl GraphicsCaptureApiHandler for FrameExtractor {
    type Flags = CaptureContext;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            tx: ctx.flags.tx,
            device_name: ctx.flags.device_name,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let width = frame.width();
        let height = frame.height();

        let result = frame
            .buffer()
            .map_err(|e| e.to_string())
            .map(|mut buffer| {
                let row_pitch = buffer.row_pitch() as usize;
                let mut data = buffer.as_raw_buffer().to_vec();
                // 只保留完整行（防对齐填充导致尾行越界）
                data.truncate(row_pitch * height as usize);
                RawFrame {
                    width,
                    height,
                    row_pitch,
                    data,
                    device_name: self.device_name.clone(),
                }
            });

        // 首帧到手即停止捕获（单帧截图场景）
        capture_control.stop();
        let _ = self.tx.send(result);
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// 定位鼠标光标当前所在的显示器（物理坐标，DPI 无关）。
///
/// 实现：`GetCursorPos` + `MonitorFromPoint(MONITOR_DEFAULTTONULL)`，
/// 再经 `Monitor::from_raw_hmonitor` 转为 windows-capture 的 `Monitor`。
/// 对应 AGENTS.md 3.4 节多显示器策略。
pub fn monitor_at_cursor() -> anyhow::Result<Monitor> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{MonitorFromPoint, HMONITOR, MONITOR_DEFAULTTONULL};
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }.context("GetCursorPos failed")?;
    let hmonitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONULL) };
    if hmonitor == HMONITOR::default() {
        anyhow::bail!("MonitorFromPoint returned null for cursor position");
    }
    Ok(Monitor::from_raw_hmonitor(hmonitor.0))
}

/// 定位鼠标光标当前所在的显示器并返回其物理矩形
/// （`GetMonitorInfoW` 的 `rcMonitor`，全屏区域、含任务栏覆盖范围）。
///
/// 覆盖层窗口按此矩形定位铺满显示器（含任务栏），选区也限制在其内。
pub fn monitor_rect_at_cursor() -> anyhow::Result<Rect> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONULL,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }.context("GetCursorPos failed")?;
    let hmonitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONULL) };
    if hmonitor == HMONITOR::default() {
        anyhow::bail!("MonitorFromPoint returned null for cursor position");
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetMonitorInfoW(hmonitor, &mut info) }
        .ok()
        .context("GetMonitorInfoW failed")?;
    let rc = info.rcMonitor;
    Ok(Rect {
        x: rc.left,
        y: rc.top,
        width: (rc.right - rc.left) as u32,
        height: (rc.bottom - rc.top) as u32,
    })
}

/// 捕获指定显示器的一帧 `Rgba16F` 数据（阻塞调用，内部起独立线程）。
///
/// `Capture::start()` 会接管调用它的线程，故在线程内执行；首帧到达后立即
/// 停止捕获，结果经 channel 传回。
///
/// * `cursor_visible` - 是否在捕获结果中包含系统光标（AGENTS.md 3.1 节：
///   光标捕获显式配置，不用 `Default`）。
///
/// # Errors
/// 捕获启动失败、线程提前退出、或超时未收到帧时返回错误。
pub fn capture_frame(monitor: Monitor, cursor_visible: bool) -> anyhow::Result<RawFrame> {
    let device_name = monitor
        .device_name()
        .unwrap_or_else(|_| String::from("\\\\.\\DISPLAY1"));

    let (tx, rx) = channel::<FrameResult>();
    // 预留一个发送端给「start 失败」的错误路径（start 成功时该端随线程退出而 drop）
    let err_tx = tx.clone();
    let handle = spawn_capture_thread(monitor, device_name, cursor_visible, tx, err_tx);

    // 轮询等待首帧：每 POLL_INTERVAL 检查一次 channel；
    // 若线程已结束仍无消息（start 失败等），提前报错而非傻等超时。
    let deadline = std::time::Instant::now() + CAPTURE_TIMEOUT;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(Ok(frame)) => {
                // 帧已到手，捕获线程正在收尾（stop 后自然退出），等待其结束
                let _ = handle.join();
                return Ok(frame);
            }
            Ok(Err(e)) => {
                let _ = handle.join();
                anyhow::bail!("capture failed: {e}");
            }
            Err(RecvTimeoutError::Timeout) => {
                if handle.is_finished() {
                    let _ = handle.join();
                    anyhow::bail!("capture thread ended without delivering a frame");
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!("capture thread dropped the channel");
            }
        }
    }
    anyhow::bail!("timed out waiting for first frame");
}

/// 起独立线程执行捕获，返回线程句柄。
///
/// `err_tx` 用于捕获启动失败时回传错误（成功后该端随线程结束而 drop）。
fn spawn_capture_thread(
    monitor: Monitor,
    device_name: String,
    cursor_visible: bool,
    tx: Sender<FrameResult>,
    err_tx: Sender<FrameResult>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let cursor = if cursor_visible {
            CursorCaptureSettings::WithCursor
        } else {
            CursorCaptureSettings::WithoutCursor
        };
        // 平台不支持切换光标捕获时（虚拟机等）降级 Default：windows-capture 对
        // 非 Default 的光标设置会检查 IsCursorCaptureEnabled 属性，虚拟机缺该属性
        // 会报 CursorConfigUnsupported，导致截图整体失败。真机仍走显式设置。
        let cursor = if cursor != CursorCaptureSettings::Default
            && !GraphicsCaptureApi::is_cursor_settings_supported().unwrap_or(true)
        {
            tracing::warn!("当前平台不支持切换光标捕获，降级为系统默认光标行为");
            CursorCaptureSettings::Default
        } else {
            cursor
        };
        let settings = Settings::new(
            monitor,
            cursor,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Rgba16F,
            CaptureContext { tx, device_name },
        );

        if let Err(e) = FrameExtractor::start(settings) {
            // start 失败：flags 已被消费，用预留的发送端把错误传回主线程
            let _ = err_tx.send(Err(format!("capture start failed: {e}")));
        }
    })
}
