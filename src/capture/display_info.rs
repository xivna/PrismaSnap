//! Windows 显示信息查询（仅 Windows 平台编译）。
//!
//! 提供两项截图 HDR 处理所需的显示器参数：
//! 1. **SDR 白点**（nit）：用 CCD API `DisplayConfigGetDeviceInfo(GET_SDR_WHITE_LEVEL)`，
//!    纯 Win32 调用，无 CoreWindow 依赖（console / 后台线程都能用）。
//!    绕开 `DisplayInformation::GetForCurrentView()` 的 CoreWindow 限制。
//! 2. **最大亮度**（nit）：用 DXGI `IDXGIOutput6::GetDesc1().MaxLuminance`，
//!    用于计算 tone map 的物理 headroom（= 最大亮度 / SDR 白点）。
//!
//! 两者均需按 GDI 设备名（如 `\\.\DISPLAY1`）匹配到目标显示器，
//! 多显示器场景下不能省（见 AGENTS.md 3.4 节）。

use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SDR_WHITE_LEVEL, DISPLAYCONFIG_SOURCE_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};

/// 把以 `\0` 结尾的 UTF-16 数组转成 Rust `String`。
fn utf16_to_string(arr: &[u16]) -> String {
    String::from_utf16_lossy(&arr.iter().copied().take_while(|&c| c != 0).collect::<Vec<u16>>())
}

/// 查询指定显示器（GDI 设备名）的 SDR 白点，单位 nit。
///
/// 通过 CCD 枚举活动显示路径，用 source 的 GDI 设备名匹配目标，
/// 再对其 target 查询 `SDRWhiteLevel`（约定 1000 = 80 nit）。
pub fn query_sdr_white_nits(device_name: &str) -> anyhow::Result<f32> {
    let mut path_count = 0u32;
    let mut mode_count = 0u32;
    unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count).ok()?; }

    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
    unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        )
        .ok()?;
    }

    for path in &paths {
        // 用 source 的 GDI 设备名匹配目标显示器
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != 0 {
            continue;
        }
        if utf16_to_string(&source.viewGdiDeviceName) != device_name {
            continue;
        }

        // 命中目标，用 target 的 adapterId / id 查询 SDR 白点
        let mut req: DISPLAYCONFIG_SDR_WHITE_LEVEL = unsafe { std::mem::zeroed() };
        req.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL;
        req.header.size = std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32;
        req.header.adapterId = path.targetInfo.adapterId;
        req.header.id = path.targetInfo.id;
        if unsafe { DisplayConfigGetDeviceInfo(&mut req.header) } != 0 {
            continue;
        }
        return Ok(req.SDRWhiteLevel as f32 / 1000.0 * 80.0);
    }

    anyhow::bail!("no active display path matched device name `{device_name}`")
}

/// 查询指定显示器（GDI 设备名）的最大亮度，单位 nit。
///
/// 通过 DXGI 枚举 adapter / output，用 `DXGI_OUTPUT_DESC.DeviceName` 匹配，
/// 再 `cast` 到 `IDXGIOutput6` 读取 `MaxLuminance`。
pub fn query_max_luminance_nits(device_name: &str) -> anyhow::Result<f32> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput6};
    use windows::core::Interface;

    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };

    for adapter_idx in 0u32.. {
        // DXGI_ERROR_NOT_FOUND 时表示适配器枚举结束
        let adapter = match unsafe { factory.EnumAdapters1(adapter_idx) } {
            Ok(a) => a,
            Err(_) => break,
        };
        for output_idx in 0u32.. {
            let output = match unsafe { adapter.EnumOutputs(output_idx) } {
                Ok(o) => o,
                Err(_) => break,
            };
            let desc = unsafe { output.GetDesc()? };
            if utf16_to_string(&desc.DeviceName) != device_name {
                continue;
            }
            let output6: IDXGIOutput6 = output.cast()?;
            let desc1 = unsafe { output6.GetDesc1()? };
            return Ok(desc1.MaxLuminance);
        }
    }

    anyhow::bail!("no DXGI output matched device name `{device_name}`")
}
