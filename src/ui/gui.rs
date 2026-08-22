//! wgpu + egui 渲染栈封装（仅 Windows 平台编译）。
//!
//! 供覆盖层窗口复用：surface 创建、egui 帧渲染完整流程
//! （`take_egui_input` → `run` → `handle_platform_output` → `tessellate` →
//! `renderer.render`），以及截图纹理上传。
//!
//! 双输出模式：
//! - **SDR 路径**（默认）：swapchain `Rgba8Unorm` + `SurfaceColorSpace::Srgb`，
//!   截图以 sRGB 纹理走 egui 绘制（与最终输出一致，所见即所得）。
//! - **HDR 路径**（HDR 屏 + surface 支持时）：swapchain `Rgba16Float` +
//!   `ExtendedSrgbLinear`（scRGB 线性，高光 >1.0 直通显示器）。
//!   合成两段式：① egui 渲染到中间纹理 `Rgba8UnormSrgb`（egui-wgpu 的
//!   `fs_main_linear_framebuffer` 路径，存储 sRGB 编码值）；② 合成 pass
//!   采样截图 scRGB 纹理（Rgba16Float，线性原样直通）+ egui 中间纹理
//!   （*Srgb 采样自动解码为线性），按预乘 alpha 混合输出到 swapchain。
//!   遮罩/UI 层因此在 linear 空间与截图混合（AGENTS.md 3.2 节）。

use std::sync::Arc;

use anyhow::Context;
use winit::event::WindowEvent;
use winit::raw_window_handle::HasDisplayHandle;
use winit::window::Window;

use crate::capture::frame::RawFrame;

/// HDR 合成 shader：截图 scRGB 直通 + egui 层预乘混合。
///
/// - `shot_tex`：Rgba16Float scRGB 截图（线性，原样输出，>1.0 高光透传）；
/// - `ui_tex`：Rgba8UnormSrgb egui 中间纹理（采样时硬件自动 sRGB→linear 解码，
///   存储为预乘 alpha 的 sRGB 值）；
/// - `ui_params.x`：UI 亮度提升系数（= 显示器 SDR 白点 nit / 80）；
/// - 混合公式（premultiplied over）：`ui.rgb * boost + shot.rgb * (1 - ui.a)`。
///
/// boost 的由来：scRGB 规定线性值 1.0 = 80 nit 固定物理亮度，而 HDR 桌面上
/// DWM 会把 SDR 内容（含 WGC 捕获帧里的桌面画面）提升到「SDR 内容亮度」滑块
/// 对应的亮度（如 268 nit → scRGB ≈ 3.35）。截图直通保留了该高值，若 UI 层
/// 按 1.0 输出就会比截图内容暗数倍（工具条发灰、框线显深，2026-08-22 实机
/// 反馈）。乘以 boost 把 UI 白拉到与截图中的 SDR 白同亮度——等价于 DWM 对
/// 普通 SDR 窗口的处理。
const COMPOSITE_SHADER: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var positions = array<vec2f, 3>(
        vec2f(-1.0, -1.0),
        vec2f(3.0, -1.0),
        vec2f(-1.0, 3.0),
    );
    let p = positions[vi];
    var out: VsOut;
    out.pos = vec4f(p, 0.0, 1.0);
    // uv.y 翻转：clip y=+1（屏幕顶部）应对应纹理 v=0（数据第一行），
    // 否则截图与 egui 层整体上下倒转
    out.uv = vec2f((p.x + 1.0) * 0.5, (1.0 - p.y) * 0.5);
    return out;
}

@group(0) @binding(0) var shot_tex: texture_2d<f32>;
@group(0) @binding(1) var ui_tex: texture_2d<f32>;
@group(0) @binding(2) var tex_sampler: sampler;
@group(0) @binding(3) var<uniform> ui_params: vec4f;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    let shot = textureSample(shot_tex, tex_sampler, in.uv);
    let ui = textureSample(ui_tex, tex_sampler, in.uv);
    return vec4f(ui.rgb * ui_params.x + shot.rgb * (1.0 - ui.a), 1.0);
}
"#;

/// HDR 合成资源（仅 HDR 输出模式存在）。
struct HdrComposite {
    /// egui 中间纹理（`Rgba8UnormSrgb`，尺寸 = 窗口物理像素）。
    ui_texture: wgpu::Texture,
    ui_texture_view: wgpu::TextureView,
    /// 截图 scRGB 纹理（`Rgba16Float`），截图上传后存在。
    scrgb_texture: Option<wgpu::Texture>,
    scrgb_view: Option<wgpu::TextureView>,
    /// UI 亮度提升系数 uniform（vec4：x = SDR 白点 nit / 80，其余保留）。
    ui_params_buf: wgpu::Buffer,
    /// 线性采样器（clamp 到边缘）。
    sampler: wgpu::Sampler,
    bind_layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    /// 合成 bind group（依赖截图视图与中间纹理视图，随二者重建）。
    bind_group: Option<wgpu::BindGroup>,
}

impl HdrComposite {
    /// 重建合成 bind group（截图上传或窗口 resize 后调用）。
    fn rebuild_bind_group(&mut self, device: &wgpu::Device) {
        let Some(scrgb_view) = &self.scrgb_view else {
            self.bind_group = None;
            return;
        };
        self.bind_group = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("composite_bind_group"),
            layout: &self.bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(scrgb_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&self.ui_texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: self.ui_params_buf.as_entire_binding(),
                },
            ],
        }));
    }
}

/// wgpu + egui 渲染栈。
pub struct GuiState {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    renderer: egui_wgpu::Renderer,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    /// HDR 合成资源；`None` 表示走 SDR 路径（swapchain 直渲 egui）。
    hdr: Option<HdrComposite>,
}

impl GuiState {
    /// 初始化 GPU 设备、surface 与 egui 渲染器。
    ///
    /// * `window` - 目标窗口（surface 按其尺寸/DPI 配置）。
    /// * `want_hdr` - 请求 HDR 输出（HDR 屏截图预览用）；surface 不支持
    ///   `Rgba16Float + ExtendedSrgbLinear` 时自动回退 SDR 路径。
    pub fn new(window: &Arc<Window>, want_hdr: bool) -> anyhow::Result<Self> {
        let instance = wgpu::Instance::default();
        let surface = instance
            .create_surface(window.clone())
            .context("创建 surface 失败")?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .context("找不到可用 GPU 适配器")?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("prismsnap_device"),
            ..Default::default()
        }))
        .context("请求 GPU 设备失败")?;

        let caps = surface.get_capabilities(&adapter);
        let hdr_supported = want_hdr
            && caps.format_capabilities.iter().any(|fc| {
                fc.format == wgpu::TextureFormat::Rgba16Float
                    && wgpu::SurfaceColorSpace::ExtendedSrgbLinear
                        .to_color_spaces()
                        .is_some_and(|flags| fc.color_spaces.contains(flags))
            });

        let (format, color_space) = if hdr_supported {
            (
                wgpu::TextureFormat::Rgba16Float,
                wgpu::SurfaceColorSpace::ExtendedSrgbLinear,
            )
        } else {
            // egui 偏好非 sRGB framebuffer（egui 输出 sRGB 编码值，写 unorm 直通；
            // *Srgb 格式会触发二次编码）。SurfaceColorSpace::Srgb 声明"值已 sRGB 编码"。
            (
                wgpu::TextureFormat::Rgba8Unorm,
                wgpu::SurfaceColorSpace::Srgb,
            )
        };
        let expected_cs = color_space
            .to_color_spaces()
            .expect("已选定的 SurfaceColorSpace 应可转 SurfaceColorSpaces");
        if !caps
            .format_capabilities
            .iter()
            .any(|fc| fc.format == format && fc.color_spaces.contains(expected_cs))
        {
            anyhow::bail!("surface 不支持 {format:?} + {color_space:?}: {:?}", caps.formats);
        }

        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        // egui 渲染目标：HDR 模式画到中间纹理（Rgba8UnormSrgb，egui-wgpu 走
        // linear_framebuffer 变体输出 linear、硬件编码回 sRGB 存储），
        // SDR 模式直接画 swapchain（Rgba8Unorm）。
        let renderer_format = if hdr_supported {
            wgpu::TextureFormat::Rgba8UnormSrgb
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        };
        let renderer = egui_wgpu::Renderer::new(&device, renderer_format, egui_wgpu::RendererOptions::default());
        let egui_ctx = egui::Context::default();
        install_cjk_font(&egui_ctx);
        let ppp = window.scale_factor() as f32;
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref() as &dyn HasDisplayHandle,
            Some(ppp),
            None,
            Some(1024),
        );

        // egui 请求重绘时转发给 winit（egui-winit 0.36 不再自动转发）
        let repaint_window = window.clone();
        egui_ctx.set_request_repaint_callback(move |_| {
            repaint_window.request_redraw();
        });

        let hdr = if hdr_supported {
            let (ui_texture, ui_texture_view) = create_ui_texture(&device, config.width, config.height);
            let (bind_layout, pipeline) = create_composite_pipeline(&device, format);
            let ui_params_buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("composite_ui_params"),
                size: 16,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            // 默认 boost = 1.0（覆盖层上传截图后由 set_ui_boost 按显示器白点覆写）
            queue.write_buffer(&ui_params_buf, 0, bytemuck::cast_slice(&[1.0f32, 0.0, 0.0, 0.0]));
            let mut hdr = HdrComposite {
                ui_texture,
                ui_texture_view,
                scrgb_texture: None,
                scrgb_view: None,
                ui_params_buf,
                sampler: device.create_sampler(&wgpu::SamplerDescriptor {
                    label: Some("composite_sampler"),
                    address_mode_u: wgpu::AddressMode::ClampToEdge,
                    address_mode_v: wgpu::AddressMode::ClampToEdge,
                    address_mode_w: wgpu::AddressMode::ClampToEdge,
                    mag_filter: wgpu::FilterMode::Linear,
                    min_filter: wgpu::FilterMode::Linear,
                    mipmap_filter: wgpu::MipmapFilterMode::Linear,
                    ..Default::default()
                }),
                bind_layout,
                pipeline,
                bind_group: None,
            };
            hdr.rebuild_bind_group(&device);
            tracing::info!("覆盖层 HDR 输出已启用: {format:?} + {color_space:?}");
            Some(hdr)
        } else {
            tracing::info!("覆盖层走 SDR 输出: {format:?} + {color_space:?}");
            None
        };

        Ok(Self {
            surface,
            device,
            queue,
            config,
            renderer,
            egui_ctx,
            egui_state,
            hdr,
        })
    }

    /// 是否启用了 HDR 输出（合成管线）。
    pub fn is_hdr(&self) -> bool {
        self.hdr.is_some()
    }

    /// 设置 HDR 合成中 UI 层的亮度提升系数。
    ///
    /// * `boost` - SDR 白点 scRGB 值（= 显示器 SDR 白点 nit / 80，由
    ///   `query_sdr_white_nits` 查询）。scRGB 线性 1.0 = 80 nit 固定物理亮度，
    ///   而 HDR 桌面把 SDR 内容提升到滑块对应亮度；不乘此系数 egui UI 会比
    ///   截图内容暗数倍（见 COMPOSITE_SHADER 注释）。仅 HDR 路径有效。
    pub fn set_ui_boost(&mut self, boost: f32) {
        let Some(hdr) = &self.hdr else {
            return;
        };
        self.queue.write_buffer(
            &hdr.ui_params_buf,
            0,
            bytemuck::cast_slice(&[boost.max(1.0), 0.0, 0.0, 0.0]),
        );
    }

    /// 把窗口事件喂给 egui（记录输入状态），返回 egui 是否消费了该事件。
    ///
    /// 注意：egui-winit 0.36 的 `on_window_event` 返回 `repaint` 标志（该事件
    /// 是否需要刷新画面），但**不会**自己触发重绘，须由调用方据此 `request_redraw`；
    /// 否则纯 egui 交互（按钮点击、文本输入）会因等不到下一次渲染而"无响应"。
    pub fn on_window_event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        let response = self.egui_state.on_window_event(window, event);
        if response.repaint {
            window.request_redraw();
        }
        response.consumed
    }

    /// 渲染一帧。
    ///
    /// * `window` - 目标窗口（取输入、回写重绘请求）。
    /// * `draw` - egui UI 绘制闭包（接收 `&mut Ui`）。
    pub fn render(&mut self, window: &Window, mut draw: impl FnMut(&mut egui::Ui)) {
        let raw_input = self.egui_state.take_egui_input(window);
        let full_output = self.egui_ctx.run_ui(raw_input, &mut draw);
        self.egui_state
            .handle_platform_output(window, full_output.platform_output);

        // 纹理增删（截图纹理、后续标注缩略图等）
        for (id, deltas) in &full_output.textures_delta.set {
            for delta in deltas {
                self.renderer.update_texture(&self.device, &self.queue, *id, delta);
            }
        }

        let ppp = self.egui_ctx.pixels_per_point();
        let primitives = self.egui_ctx.tessellate(full_output.shapes, ppp);
        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point: ppp,
        };

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });

        // 顶点/索引缓冲上传（egui-wgpu 0.36 必须在 render 前显式调用，
        // 否则渲染访问未上传的 buffer，Vulkan 后端会驱动级崩溃）
        let user_cmd_bufs = self.renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &primitives,
            &screen_descriptor,
        );

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            other => {
                tracing::warn!("get_current_texture: {other:?}");
                return;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        if let Some(hdr) = &self.hdr {
            // HDR 两段式：pass 1 egui → 中间纹理；pass 2 合成 → swapchain
            {
                let rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("egui_ui_pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &hdr.ui_texture_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                // forget_lifetime：wgpu 官方模式，pass 不交给外部即安全
                self.renderer.render(
                    &mut rp.forget_lifetime(),
                    &primitives,
                    &screen_descriptor,
                );
            }
            if let Some(bind_group) = &hdr.bind_group {
                let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("composite_pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                rp.set_pipeline(&hdr.pipeline);
                rp.set_bind_group(0, bind_group, &[]);
                rp.draw(0..3, 0..1);
            }
        } else {
            {
                let rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("prismsnap_pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                // forget_lifetime：wgpu 官方模式，pass 不交给外部即安全
                self.renderer.render(
                    &mut rp.forget_lifetime(),
                    &primitives,
                    &screen_descriptor,
                );
            }
        }
        self.queue
            .submit(user_cmd_bufs.into_iter().chain([encoder.finish()]));

        // 销毁纹理须在 submit 之后（可能仍被本帧命令引用）
        for id in &full_output.textures_delta.free {
            self.renderer.free_texture(id);
        }

        window.pre_present_notify();
        self.queue.present(frame);
    }

    /// 窗口尺寸变化后重配 surface（HDR 模式下同步重建中间纹理）。
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        if let Some(hdr) = &mut self.hdr {
            (hdr.ui_texture, hdr.ui_texture_view) = create_ui_texture(&self.device, width, height);
            hdr.rebuild_bind_group(&self.device);
        }
    }

    /// egui 上下文（注册纹理等）。
    pub fn egui_ctx(&self) -> &egui::Context {
        &self.egui_ctx
    }

    /// 把 RGBA8 图像上传为 GPU 纹理并注册到 egui，返回
    /// `(TextureId, Texture, TextureView)`。
    ///
    /// 纹理为 `Rgba8Unorm`（非 `*Srgb`）：图像已是 sRGB 编码值，egui-wgpu
    /// 假设纹理"NOT sRGB-aware"（采样不转换、原样输出）。若用 `*Srgb` 格式，
    /// wgpu 采样时会自动解码为 linear，egui 再当 sRGB 输出 → 双重 gamma、
    /// 预览偏亮偏饱和。调用方须持有返回的 `Texture`/`TextureView` 直到不再
    /// 使用该 `TextureId`。仅 SDR 路径使用（HDR 路径截图走
    /// [`Self::upload_scrgb_texture`]）。
    pub fn upload_texture(
        &mut self,
        img: &image::RgbaImage,
    ) -> (egui::TextureId, wgpu::Texture, wgpu::TextureView) {
        let (width, height) = img.dimensions();
        let texture_size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("screenshot"),
            size: texture_size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(img.as_raw()),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * width),
                rows_per_image: None,
            },
            texture_size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let id = self
            .renderer
            .register_native_texture(&self.device, &view, wgpu::FilterMode::Linear);
        (id, texture, view)
    }

    /// 把 scRGB 原始帧（f16 小端字节流）上传为 `Rgba16Float` 纹理，
    /// 供合成 pass 直通显示（HDR 路径，线性值零转换）。
    ///
    /// 纹理与视图存于 `GuiState` 内部（合成管线持有视图引用），
    /// 上传前会紧凑化数据并 256 字节行对齐（wgpu 要求）。
    pub fn upload_scrgb_texture(&mut self, raw: &RawFrame) -> anyhow::Result<()> {
        let hdr = self.hdr.as_mut().context("HDR 合成管线未启用")?;
        let data = raw.compact_rgba16f_data();
        let bytes_per_row = (raw.width as usize * 8).next_multiple_of(256) as u32;
        let texture_size = wgpu::Extent3d {
            width: raw.width,
            height: raw.height,
            depth_or_array_layers: 1,
        };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("screenshot_scrgb"),
            size: texture_size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&data),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: None,
            },
            texture_size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        hdr.scrgb_texture = Some(texture);
        hdr.scrgb_view = Some(view);
        hdr.rebuild_bind_group(&self.device);
        Ok(())
    }
}

/// 创建 egui 中间纹理（`Rgba8UnormSrgb`，尺寸 = 窗口物理像素）。
fn create_ui_texture(device: &wgpu::Device, width: u32, height: u32) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("egui_ui_texture"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

/// 创建合成管线与 bind group layout。
fn create_composite_pipeline(
    device: &wgpu::Device,
    output_format: wgpu::TextureFormat,
) -> (wgpu::BindGroupLayout, wgpu::RenderPipeline) {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("composite_shader"),
        source: wgpu::ShaderSource::Wgsl(COMPOSITE_SHADER.into()),
    });
    let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("composite_bind_layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(16),
                },
                count: None,
            },
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("composite_layout"),
        bind_group_layouts: &[Some(&bind_layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("composite_pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: output_format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    (bind_layout, pipeline)
}

/// 按配置主题应用 egui visuals（设置窗口与覆盖层工具条共用）。
pub fn apply_theme(ctx: &egui::Context, theme: crate::config::Theme) {
    ctx.set_visuals(match theme {
        crate::config::Theme::Light => egui::Visuals::light(),
        crate::config::Theme::Dark => egui::Visuals::dark(),
    });
}

/// 设置窗口 Apple 风配色（iOS/macOS 系统色板，跟随主题）。
pub struct Palette {
    /// 内容区底色（设置窗口背景）。
    pub page_bg: egui::Color32,
    /// 左侧栏底色（比内容区略深一档，形成层次）。
    pub sidebar_bg: egui::Color32,
    /// 侧栏导航项悬停底色。
    pub nav_hover: egui::Color32,
    /// 侧栏导航项选中底色。
    pub nav_selected: egui::Color32,
    /// 分组卡片底色。
    pub card_bg: egui::Color32,
    /// 卡片描边。
    pub card_stroke: egui::Color32,
    /// 卡片内行分隔线。
    pub separator: egui::Color32,
    /// 次级文字（说明文字、未选中分段）。
    pub secondary: egui::Color32,
    /// 弱控件底色（键帽、分段选择器槽、toggle 槽）。
    pub control_bg: egui::Color32,
    /// 强调色（主按钮、toggle 开启态）。
    pub accent: egui::Color32,
}

/// 按主题取配色：浅色 = 浅灰页面 + 白卡片；深色 = 近黑页面 + 提亮卡片。
///
/// 采用固定 iOS 色板而非 egui 默认 visuals——dark 主题默认 `window_fill`
/// 仅 gray(27)，卡片层次会反转。仅用于设置窗口（覆盖层工具条随主题
/// 自行取色，见 `toolbar::toolbar_ui`）。
pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            page_bg: egui::Color32::from_rgb(24, 24, 27),
            sidebar_bg: egui::Color32::from_rgb(30, 30, 34),
            nav_hover: egui::Color32::from_white_alpha(10),
            nav_selected: egui::Color32::from_white_alpha(26),
            card_bg: egui::Color32::from_rgb(43, 43, 48),
            card_stroke: egui::Color32::from_white_alpha(24),
            separator: egui::Color32::from_white_alpha(20),
            secondary: egui::Color32::from_rgb(152, 152, 160),
            control_bg: egui::Color32::from_rgb(56, 56, 62),
            accent: egui::Color32::from_rgb(10, 132, 255),
        }
    } else {
        Palette {
            page_bg: egui::Color32::from_rgb(242, 242, 247),
            sidebar_bg: egui::Color32::from_rgb(233, 233, 238),
            nav_hover: egui::Color32::from_black_alpha(8),
            nav_selected: egui::Color32::from_black_alpha(20),
            card_bg: egui::Color32::WHITE,
            card_stroke: egui::Color32::from_black_alpha(28),
            separator: egui::Color32::from_black_alpha(16),
            secondary: egui::Color32::from_rgb(110, 110, 118),
            control_bg: egui::Color32::from_rgb(228, 228, 234),
            accent: egui::Color32::from_rgb(0, 122, 255),
        }
    }
}

/// 加载系统中文字体字节（进程内缓存，避免每次截图重复读盘）。
///
/// 依次尝试 Windows 自带字体：微软雅黑（`msyh.ttc`，ttc index 0 为常规体）、
/// 黑体（`simhei.ttf`）；都找不到返回 `None`（界面中文退化为方块，不崩溃）。
fn cjk_font_bytes() -> Option<&'static [u8]> {
    static FONT: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();
    FONT.get_or_init(|| {
        const CANDIDATES: [&str; 4] = [
            r"C:\Windows\Fonts\msyh.ttc",
            r"C:\Windows\Fonts\msyh.ttf",
            r"C:\Windows\Fonts\simhei.ttf",
            r"C:\Windows\Fonts\simsun.ttc",
        ];
        for path in CANDIDATES {
            match std::fs::read(path) {
                Ok(bytes) => {
                    tracing::info!("已加载系统中文字体: {path}");
                    return Some(bytes);
                }
                Err(_) => continue,
            }
        }
        tracing::warn!("未找到系统中文字体，界面中文可能显示为方块");
        None
    })
    .as_deref()
}

/// 把系统中文字体追加为 egui 的 fallback 字体（英文/数字仍用默认字体）。
fn install_cjk_font(ctx: &egui::Context) {
    let Some(bytes) = cjk_font_bytes() else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "cjk".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(bytes)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("cjk".to_owned());
    }
    ctx.set_fonts(fonts);
}
