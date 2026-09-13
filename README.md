# PrismaSnap

Windows 11 便携式截图工具：HDR 正确、标注顺手、AI 提取与翻译。

- **HDR 友好**：系统开启 HDR 时截图不过曝、不发灰，预览与导出行为对齐 Windows 自带截图
- **即时标注**：矩形 / 箭头 / 画笔 / 马赛克（像素化、模糊、纯色）/ 文字，全程所见即所得
- **AI 增强**：一键提取图中文字（OCR，可插拔 RapidOCR 插件）；翻译原位覆盖在原文字之上
- **本地优先**：AI 接口兼容 OpenAI 格式，可接 llama.cpp / Ollama 等本地服务
- **完全便携**：解压即用，配置 / 日志 / 截图都在程序目录，不写注册表

## 使用

系统要求 Windows 11 x64。下载 `PrismaSnap_1.0.0_portable.zip` 解压到任意目录，
双击 `PrismaSnap.exe`：启动后驻留托盘，默认 `Ctrl+Alt+A` 触发截图。
完整说明见 [`docs/使用说明.md`](docs/使用说明.md)（压缩包内附同名文件）。

可选安装 OCR 插件包：解压出 `plugins/ocr/` 合并到程序目录即启用高精度识别（见使用说明第九节）。

## 构建与测试

```bash
# 目标平台 Windows 11 x86_64 (MSVC)，WSL2 交叉编译（环境配置见 AGENTS.md 5.1）
cargo build --target x86_64-pc-windows-msvc --release

# 纯逻辑单元测试（WSL2 可直接运行）
cargo test
```

## 发布打包

```bash
scripts/package.sh [输出目录]   # 默认输出到 D:\Download
```

产出两个包：主包（exe + 使用说明 + 许可证）与 OCR 插件包（`plugins/ocr/` 四件套，
含 det/rec 模型 SHA256 复核）。

## 文档

| 文件 | 内容 |
| :--- | :--- |
| [`docs/使用说明.md`](docs/使用说明.md) | 用户手册（安装 / 快捷键 / 标注 / AI 配置 / FAQ） |
| [`AGENTS.md`](AGENTS.md) | 需求与设计规格、架构决策与协作规范 |
| [`PROGRESS.md`](PROGRESS.md) | 开发进度与决策记录 |
| [`TASKS.md`](TASKS.md) | 任务清单 |

## 开源协议

[GPL-3.0-only](LICENSE)。
