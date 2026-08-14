//! wgpu + egui 渲染栈封装（仅 Windows 平台编译）。
//!
//! 供覆盖层窗口复用：surface 创建、egui 帧渲染完整流程
//! （`take_egui_input` → `run` → `handle_platform_output` → `tessellate` →
//! `renderer.render`），以及截图纹理上传。
//!
//! 当前 MVP 走 **SDR 路径**（`Rgba8UnormSrgb` + `SurfaceColorSpace::Srgb`），
//! 显示经 HDR 转换后的 sRGB 截图（与最终输出一致，所见即所得）；
//! HDR swapchain（`Rgba16Float` + `ExtendedSrgbLinear`，scRGB 直通）已由
//! `wgpu_hdr_demo` 验证机制，待实机确认后接入合成管线（见 PROGRESS.md）。

use std::sync::Arc;

use anyhow::Context;
use winit::event::WindowEvent;
use winit::raw_window_handle::HasDisplayHandle;
use winit::window::Window;

/// wgpu + egui 渲染栈。
pub struct GuiState {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    renderer: egui_wgpu::Renderer,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
}

impl GuiState {
    /// 初始化 GPU 设备、surface 与 egui 渲染器。
    ///
    /// * `window` - 目标窗口（surface 按其尺寸/DPI 配置）。
    pub fn new(window: &Arc<Window>) -> anyhow::Result<Self> {
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
        // egui 偏好非 sRGB framebuffer（egui 输出 sRGB 编码值，写 unorm 直通；
        // *Srgb 格式会触发二次编码）。SurfaceColorSpace::Srgb 声明"值已 sRGB 编码"。
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let color_space = wgpu::SurfaceColorSpace::Srgb;
        if !caps
            .format_capabilities
            .iter()
            .any(|fc| fc.format == format && fc.color_spaces.contains(wgpu::SurfaceColorSpaces::SRGB))
        {
            anyhow::bail!("surface 不支持 Rgba8Unorm + Srgb: {:?}", caps.formats);
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

        let renderer = egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());
        let egui_ctx = egui::Context::default();
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

        Ok(Self {
            surface,
            device,
            queue,
            config,
            renderer,
            egui_ctx,
            egui_state,
        })
    }

    /// 把窗口事件喂给 egui（记录输入状态），返回 egui 是否消费了该事件。
    pub fn on_window_event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        self.egui_state.on_window_event(window, event).consumed
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
        self.queue
            .submit(user_cmd_bufs.into_iter().chain([encoder.finish()]));

        // 销毁纹理须在 submit 之后（可能仍被本帧命令引用）
        for id in &full_output.textures_delta.free {
            self.renderer.free_texture(id);
        }

        window.pre_present_notify();
        self.queue.present(frame);
    }

    /// 窗口尺寸变化后重配 surface。
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    /// egui 上下文（注册纹理等）。
    pub fn egui_ctx(&self) -> &egui::Context {
        &self.egui_ctx
    }

    /// 把 RGBA8 图像上传为 GPU 纹理并注册到 egui，返回
    /// `(TextureId, Texture, TextureView)`。
    ///
    /// 纹理为 `Rgba8UnormSrgb`（图像已是 sRGB 编码值），egui 按 sRGB 纹理采样；
    /// 调用方须持有返回的 `Texture`/`TextureView` 直到不再使用该 `TextureId`。
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
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
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
}
