//! wgpu HDR 渲染输出最小 demo（Phase 1 遗留验收项，对应 AGENTS.md 3.2 节）。
//!
//! 验证目标：预览/编辑窗口本身在 HDR 屏幕上正确显示（不只验证捕获端）。
//! 实现：swapchain 用 `Rgba16Float` + `SurfaceColorSpace::ExtendedSrgbLinear`
//! （即 scRGB 线性，DXGI `DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709`），
//! shader 直接输出线性 scRGB 值（>1.0 高光透传，不做任何 tone map），
//! 由 DXGI 直接把信号交给 HDR 显示器——所见即所得。
//!
//! 屏幕内容是一张「scRGB 测试卡」：
//! - 上半：线性渐变 0 → 4.0（x=0.25 处蓝色竖线标记 1.0 即 SDR 白点）
//! - 下半：5×5 色块矩阵，列 = 亮度 0.25/0.5/1.0/2.0/4.0，行 = 白/红/绿/蓝/洋红
//!
//! 实机验收标准（HDR 屏 + 系统 HDR 开启）：
//! - 1.0 白块亮度 ≈ 普通 SDR 窗口的纯白
//! - 2.0 / 4.0 块明显比 1.0 更亮（亮区渐变连续、无裁切）
//! - 若回退 SDR（Rgba8UnormSrgb）：2.0/4.0 与 1.0 同亮度（高光裁白），报告里会说明
//!
//! 报告写入 exe 同目录 `wgpu_hdr_report.txt`，Esc 退出。

#[cfg(target_os = "windows")]
mod imp {
    use std::borrow::Cow;
    use std::fs;
    use std::sync::Arc;

    use anyhow::Context;
    use winit::application::ApplicationHandler;
    use winit::event::{ElementState, KeyEvent, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::keyboard::{KeyCode, PhysicalKey};
    use winit::window::{Window, WindowId};

    /// 测试卡渲染管线使用的三角形顶点着色器（全屏三角形）。
    const TEST_CARD_SHADER: &str = r#"
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
    out.uv = (p + 1.0) * 0.5;
    return out;
}

// 色块矩阵亮度等级：列 0..5 对应 0.25 / 0.5 / 1.0 / 2.0 / 4.0
fn block_luminance(col: i32) -> f32 {
    if col == 0 { return 0.25; }
    if col == 1 { return 0.5; }
    if col == 2 { return 1.0; }
    if col == 3 { return 2.0; }
    return 4.0;
}

// 色块矩阵颜色：行 0..5 对应 白 / 红 / 绿 / 蓝 / 洋红
fn block_color(row: i32, lv: f32) -> vec3f {
    if row == 0 { return vec3f(lv, lv, lv); }
    if row == 1 { return vec3f(lv, 0.0, 0.0); }
    if row == 2 { return vec3f(0.0, lv, 0.0); }
    if row == 3 { return vec3f(0.0, 0.0, lv); }
    return vec3f(lv, 0.0, lv);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    let uv = in.uv;
    if uv.y < 0.5 {
        // 上半：线性渐变 0 → 4.0，x=0.25 处画 SDR 白点标记线
        let v = uv.x * 4.0;
        if abs(uv.x - 0.25) < 0.004 {
            return vec4f(0.3, 0.5, 1.0, 1.0);
        }
        return vec4f(v, v, v, 1.0);
    }
    // 下半：5x5 色块矩阵（黑缝分隔）
    let row = i32(floor((uv.y - 0.5) / 0.1));
    let col = i32(floor(uv.x * 5.0));
    let fx = fract(uv.x * 5.0);
    let fy = fract((uv.y - 0.5) / 0.1);
    if fx < 0.04 || fy < 0.04 {
        return vec4f(0.0, 0.0, 0.0, 1.0);
    }
    let lv = block_luminance(col);
    return vec4f(block_color(row, lv), 1.0);
}
"#;

    /// 渲染状态：surface + 管线。
    struct State {
        surface: wgpu::Surface<'static>,
        device: wgpu::Device,
        queue: wgpu::Queue,
        config: wgpu::SurfaceConfiguration,
        pipeline: wgpu::RenderPipeline,
        /// 汇总报告文本（退出时写文件）。
        report: String,
    }

    impl State {
        fn new(window: Arc<Window>) -> anyhow::Result<Self> {
            let mut report = String::new();
            let log = |r: &mut String, line: &str| {
                println!("{line}");
                r.push_str(line);
                r.push('\n');
            };

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
            log(&mut report, &format!("adapter: {}", adapter.get_info().name));

            let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("hdr_demo_device"),
                ..Default::default()
            }))
            .context("请求 GPU 设备失败")?;

            // 能力检测：优先 Rgba16Float + ExtendedSrgbLinear（scRGB 线性 HDR），
            // 不可用回退 Rgba8UnormSrgb + Srgb（SDR）。
            let caps = surface.get_capabilities(&adapter);
            log(&mut report, "surface format 能力:");
            for fc in &caps.format_capabilities {
                log(&mut report, &format!("  {:?}: {:?}", fc.format, fc.color_spaces));
            }

            let hdr_fc = caps.format_capabilities.iter().find(|fc| {
                fc.format == wgpu::TextureFormat::Rgba16Float
                    && wgpu::SurfaceColorSpace::ExtendedSrgbLinear
                        .to_color_spaces()
                        .is_some_and(|flags| fc.color_spaces.contains(flags))
            });

            let (format, color_space, mode_desc) = if hdr_fc.is_some() {
                (
                    wgpu::TextureFormat::Rgba16Float,
                    wgpu::SurfaceColorSpace::ExtendedSrgbLinear,
                    "HDR（scRGB 线性，高光 >1.0 直通）",
                )
            } else {
                (
                    wgpu::TextureFormat::Rgba8UnormSrgb,
                    wgpu::SurfaceColorSpace::Srgb,
                    "SDR 回退（Rgba8UnormSrgb，>1.0 裁白）",
                )
            };
            log(&mut report, &format!("选择: {format:?} + {color_space:?} → {mode_desc}"));

            // 显示器 HDR 信息（advisory，非 Option）
            let hdr_info = surface.display_hdr_info(&adapter);
            log(&mut report, "display_hdr_info:");
            if let Some(lum) = hdr_info.luminance {
                log(&mut report, &format!("  max_nits: {:?}", lum.max_nits));
                log(&mut report, &format!("  max_full_frame_nits: {:?}", lum.max_full_frame_nits));
                log(&mut report, &format!("  min_nits: {:?}", lum.min_nits));
                log(&mut report, &format!("  sdr_white_nits: {:?}", lum.sdr_white_nits));
            } else {
                log(&mut report, "  luminance: None");
            }
            log(&mut report, &format!("  coarse: {:?}", hdr_info.coarse));
            log(&mut report, &format!("  bits_per_color: {:?}", hdr_info.bits_per_color));

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

            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("hdr_test_card"),
                source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(TEST_CARD_SHADER)),
            });
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("hdr_test_layout"),
                bind_group_layouts: &[],
                immediate_size: 0,
            });
            // 注意：shader 输出的是线性 scRGB 值。
            // - Rgba16Float（linear 解释）：值原样显示，>1.0 高光直达显示器；
            // - Rgba8UnormSrgb：硬件自动做 sRGB 编码（linear → sRGB），>1.0 裁白。
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("hdr_test_pipeline"),
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
                        format,
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

            log(&mut report, &format!("窗口大小: {size:?}"));
            Ok(Self {
                surface,
                device,
                queue,
                config,
                pipeline,
                report,
            })
        }

        fn resize(&mut self, width: u32, height: u32) {
            if width == 0 || height == 0 {
                return;
            }
            self.config.width = width;
            self.config.height = height;
            self.surface.configure(&self.device, &self.config);
        }

        fn render(&self) {
            let frame = match self.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(f) => f,
                wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
                wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                    self.surface.configure(&self.device, &self.config);
                    return;
                }
                other => {
                    eprintln!("get_current_texture: {other:?}");
                    return;
                }
            };
            let view = frame
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            {
                let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("hdr_test_pass"),
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
                rp.set_pipeline(&self.pipeline);
                rp.draw(0..3, 0..1);
            }
            self.queue.submit([encoder.finish()]);
            self.queue.present(frame);
        }

        /// 把报告写入 exe 同目录。
        fn write_report(&self) {
            let path = match prismsnap::utils::paths::exe_dir() {
                Ok(dir) => dir.join("wgpu_hdr_report.txt"),
                Err(_) => std::path::PathBuf::from("wgpu_hdr_report.txt"),
            };
            if let Err(e) = fs::write(&path, &self.report) {
                eprintln!("写入报告失败: {e}");
            } else {
                println!("报告已写入: {}", path.display());
            }
        }
    }

    /// 事件循环应用状态。
    struct App {
        state: Option<State>,
    }

    impl ApplicationHandler for App {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.state.is_some() {
                return;
            }
            let window = match event_loop.create_window(Window::default_attributes().with_title(
                "PrismaSnap wgpu HDR demo（Esc 退出，报告 wgpu_hdr_report.txt）",
            )) {
                Ok(w) => Arc::new(w),
                Err(e) => {
                    eprintln!("创建窗口失败: {e}");
                    event_loop.exit();
                    return;
                }
            };
            match State::new(window) {
                Ok(s) => self.state = Some(s),
                Err(e) => {
                    eprintln!("初始化失败: {e:#}");
                    event_loop.exit();
                }
            }
        }

        fn window_event(
            &mut self,
            event_loop: &ActiveEventLoop,
            _id: WindowId,
            event: WindowEvent,
        ) {
            match event {
                WindowEvent::CloseRequested => event_loop.exit(),
                WindowEvent::Resized(size) => {
                    if let Some(state) = &mut self.state {
                        state.resize(size.width, size.height);
                    }
                }
                WindowEvent::KeyboardInput {
                    event:
                        KeyEvent {
                            physical_key: PhysicalKey::Code(KeyCode::Escape),
                            state: ElementState::Pressed,
                            ..
                        },
                    ..
                } => event_loop.exit(),
                WindowEvent::RedrawRequested => {
                    if let Some(state) = &self.state {
                        state.render();
                    }
                }
                _ => {}
            }
        }
    }

    /// demo 入口（console 程序，直接打印报告）。
    pub fn run() -> anyhow::Result<()> {
        println!("=== PrismaSnap wgpu HDR demo ===");
        let event_loop = EventLoop::new().context("创建事件循环失败")?;
        event_loop.set_control_flow(ControlFlow::Poll);
        let mut app = App { state: None };
        let res = event_loop.run_app(&mut app);
        if let Some(state) = app.state {
            state.write_report();
        }
        res.context("事件循环异常退出")
    }
}

#[cfg(target_os = "windows")]
fn main() -> anyhow::Result<()> {
    imp::run()
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("wgpu_hdr_demo 仅支持 Windows（x86_64-pc-windows-msvc）");
}
