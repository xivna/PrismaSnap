这两条曲线的差别正好解释了你们观察到的现象：**当前方案**在 x∈[0,1] 整段都乘以 0.7，所以哪怕是纯 SDR 内容（没有任何高光）也被整体压暗、发灰；**建议方案**在 knee（比如 0.65）以下保持恒等映射，只在接近白点和真正的 HDR 高光区间才开始压缩——大部分画面完全不受影响，只有亮部被"挤"进剩余空间。

## 1 + 3：不是物理矛盾，是曲线设计问题

8-bit 256 级要同时服务"SDR 正确"和"HDR 高光有细节"，信息论上确实要做取舍——但取舍的对象应该只是**顶部一小段**（比如亮度前 30~40%），而不是整个动态范围。你们现在的方案本质是"全局线性压暗 + 高光对数滚降"，压暗是均匀施加的，这是发灰的根源，不是 headroom 本身的错。

工业界处理这个问题（HDR→SDR 显示适配）的标准做法是 **knee/shoulder 曲线**：低于 knee 点斜率恒为 1（原样输出），只在 knee 到 headroom 之间做平滑压缩。ITU-R BT.2390（EETF）、ACES 的 RRT、游戏里常用的 Hable/Uncharted2 filmic curve、Reinhard extended，思路都是这个（具体参数细节我是凭记忆写的，建议你们对照官方文档核实，我这边没有搜索工具可以现查）。

一个可以直接抄的实现：

```rust
/// x <= knee：恒等映射，SDR 内容不受影响；
/// x > knee：平滑压缩到 [knee, 1.0]，headroom 决定最大过曝倍数被吸收到哪。
fn tone_map(x: f32, knee: f32, headroom: f32) -> f32 {
    if x <= knee {
        x
    } else {
        let span = (headroom - knee).max(1e-4);
        let t = ((x - knee) / span).min(1.0);
        knee + (1.0 - knee) * (t * (2.0 - t)) // ease-out，可换成 Reinhard: t/(1+t) 等
    }
}
```

knee 建议 0.6~0.75 之间试；headroom 建议不要写死 6，而是用显示器实际报告的最大亮度算出来（`IDXGIOutput6::GetDesc1()` 里的 `MaxLuminance`，单位 nit，除以 SDR 白点 nit 得到 headroom），这样不同显示器给出的压缩范围是物理准确的，而不是拍脑袋的常数。

**另外一个容易被忽略、但会实际影响观感的问题**：你们现在是对 R/G/B 三个通道分别独立做 tone map（`linear_to_srgb(r,...)`、`(g,...)`、`(b,...)` 各算各的）。这对饱和度高的亮部（比如高亮的红色 UI 元素、镜面反射带色高光）会造成**色相偏移**——因为三个通道各自被压缩的比例不一样。更标准的做法是算亮度（Y）、只对 Y 做 tone map，再把同一个缩放系数应用回三个通道，这样色相/饱和度比例保持不变：

```rust
fn linear_to_srgb_preserve_hue(r: f32, g: f32, b: f32, sdr_white_scrgb: f32, knee: f32, headroom: f32) -> [u8; 3] {
    let (rn, gn, bn) = (r / sdr_white_scrgb, g / sdr_white_scrgb, b / sdr_white_scrgb);
    let y = 0.2126 * rn + 0.7152 * gn + 0.0722 * bn;
    let y_mapped = tone_map(y.max(0.0), knee, headroom);
    let scale = if y > 1e-6 { y_mapped / y } else { 1.0 };
    let clamp01 = |v: f32| (v * scale).clamp(0.0, 1.0);
    [gamma(clamp01(rn)), gamma(clamp01(gn)), gamma(clamp01(bn))]
}
```

（注意：极端饱和色在按亮度缩放后单通道仍可能超 1.0 被裁到白，这是所有"保色相"tone map 都会遇到的边界情况，一般再加一步简单去饱和兜底即可，先跑起来看效果，不一定要一次做到完美。）

## 2：要不要输出 HDR 格式

我的建议是**不要用 HDR 格式替换 SDR PNG，而是并行提供**——默认剪贴板 / 默认保存仍然是 tone-map 过的 SDR PNG（保证微信、Office、绝大多数图床的兼容性），同时提供一个可选的"保存 HDR 版本"（JPEG XR 或 AVIF+HDR metadata），给需要原始高光数据的用户用。Xbox Game Bar 的 HDR 截图功能就是这个思路：默认给 SDR，同时可选生成 HDR 文件。这样不用在"兼容性"和"数据完整"之间二选一。

16-bit PNG 我不太推荐作为主输出：多数应用粘贴/预览时仍按 8-bit 处理，拿不到该有的收益，却要多处理色彩空间标签的问题。AVIF(HDR) 效率和生态都在变好，但旧软件遇到不认识 HDR metadata 的 AVIF 时经验通常比 tone-map 过的 SDR 更差（可能直接花屏或读不出）。JXR 是 Windows 自家生态用得最多的，但第三方软件几乎不支持。

## 4：主流工具怎么做的

这块我没有把握给你确切、可验证的答案——我记忆里 Windows 自带截图工具（Snipping Tool / Win+Shift+S）在近两年加过 HDR 截图相关处理，Snipaste 等第三方工具社区里也有过关于 HDR 发灰/过曝的讨论，但具体算法细节我不敢保证准确，而且这块变化比较快，我这个环境里也没有联网搜索工具可以帮你现查。如果你希望拿到确切、有出处的说法，建议：
- 直接翻这些工具的 GitHub issue / 更新日志（Snipaste、ShareX 都是活跃维护的，HDR 相关的 issue 一般讨论得很细）；
- 或者换一个能联网搜索的对话/工具帮你查最新情况。

不过可以给你一个方向性的判断：只要是"必须在 8-bit SDR 里同时呈现 SDR 内容和 HDR 高光"这个问题，最终收敛的方案基本都是 knee/shoulder 类曲线，而不是全局线性压暗——这个结论应该是比较稳的，不太会因为工具不同而不同。

## 5：SDR 白点获取（绕开 CoreWindow 限制）

`DisplayInformation.GetForCurrentView()` 报错正是因为它要求当前线程关联一个 `CoreWindow`，console 程序天然没有。有一个不依赖 WinRT/CoreWindow 的 Win32 原生 API 可以解决：**CCD（Connecting and Configuring Displays）API** 里的 `DisplayConfigGetDeviceInfo`，请求类型 `DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL`。这是纯 Win32 调用，控制台程序、后台服务都能用：

```rust
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SDR_WHITE_LEVEL, QDC_ONLY_ACTIVE_PATHS,
};

// 1. 枚举当前活动的显示路径
let mut path_count = 0u32;
let mut mode_count = 0u32;
unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)?; }
let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> = vec![Default::default(); path_count as usize];
let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> = vec![Default::default(); mode_count as usize];
unsafe {
    QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut path_count, paths.as_mut_ptr(),
        &mut mode_count, modes.as_mut_ptr(), None)?;
}

// 2. 对目标显示器的 path，用 targetInfo.adapterId / targetInfo.id 查询白点
fn query_sdr_white_level(adapter_id: windows::Win32::Foundation::LUID, target_id: u32) -> windows::core::Result<f32> {
    let mut req = DISPLAYCONFIG_SDR_WHITE_LEVEL::default();
    req.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL;
    req.header.size = std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32;
    req.header.adapterId = adapter_id;
    req.header.id = target_id;
    unsafe { DisplayConfigGetDeviceInfo(&mut req.header as *mut _)?; }
    // 约定：SDRWhiteLevel 是以 1000 = 80 nit 为单位的比例值
    Ok(req.SDRWhiteLevel as f32 / 1000.0 * 80.0)
}
```

需要注意两点：
- **匹配到你的目标显示器**：`paths` 里每条路径的 `sourceInfo` 对应一个 GDI 设备名（比如 `\\.\DISPLAY1`），你需要用 `GetMonitorInfoW` 拿到 `windows-capture` 里 `Monitor` 对应的 `szDevice`，两边对上号，再用那条 path 的 `targetInfo.adapterId`/`targetInfo.id` 去查白点。多显示器场景下这一步不能省。
- **这个值会实时变化**：Windows 11 设置里"HDR 内容中的 SDR 内容亮度"滑块调整后，这个值会跟着变，建议截图时现查一次，而不是启动时缓存一份。

这个方案我是凭记忆写的（结构体字段名、`r#type` 是不是要这么写、`windows` crate 里具体的模块路径和版本要求），细节上有出入的概率不低，建议你落地前对照 docs.rs 上 `windows` crate 的 `Win32::Devices::Display` 模块核实一遍具体签名，而不是直接照抄编译。作为 fallback 兜底逻辑（查询失败时用固定值），你们现在代码里已经有了，这个思路可以保留。