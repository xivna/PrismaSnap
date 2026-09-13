//! RapidOCR 插件引擎（PP-OCR ONNX 模型，`plugins/ocr/` 即放即用，见 AGENTS.md 3.8 节）。
//!
//! - 文件规则：`det.onnx`（PP-OCRv6 small 检测，约 9.9MB）+ `rec.onnx`
//!   （同系列识别，约 21MB）+ `keys.txt`（`ppocrv6_dict.txt`，须与 rec 配套，
//!   错配直接报错、绝不输出乱码）+ `onnxruntime.dll`（须与 `ort` 大版本配对），
//!   四齐才可用（模型版本见 AGENTS.md 3.8 节，管线与版本无关）；
//! - 推理（仅 Windows）：`ort` 仅 `load-dynamic`，经 [`ort::init_from`] 从插件
//!   目录加载 DLL；会话进程内全局缓存，`Session::run` 需 `&mut` 故推理全程
//!   持同一把 `Mutex` 串行；
//! - 前后处理对齐 RapidOCR 官方（`DetPreProcess` min/736 + mean/std 0.5、
//!   `DBPostProcess` thresh 0.3 / box 0.5 / unclip 1.6 / 2x2 膨胀、CTC 贪心解码），
//!   仅两处简化：① 连通域取外接矩形（截图多为水平文字，与 minAreaRect 实测
//!   一致，省掉 pyclipper/shapely 依赖）；② rec 逐行单张推理（暂不组批）。
//!
//! 同步阻塞调用，调用方必须包在 `spawn_blocking` 里（见工具条后台任务），
//! 勿阻塞 UI 线程。

use std::path::{Path, PathBuf};

use super::{OcrEngine, TextRegion};
#[cfg(any(target_os = "windows", test))]
use crate::ocr::BBox;

/// 文本检测模型文件名（`>5M`，插件目录提供）。
pub const DET_MODEL_FILE: &str = "det.onnx";
/// 文本识别模型文件名（`>5M`，插件目录提供）。
pub const REC_MODEL_FILE: &str = "rec.onnx";
/// 识别字典文件名。
pub const KEYS_FILE: &str = "keys.txt";
/// ONNX Runtime 动态库文件名（`ort` 仅 `load-dynamic`，运行时从插件目录加载）。
#[cfg(target_os = "windows")]
pub const RUNTIME_LIB_FILE: &str = "onnxruntime.dll";
/// 非 Windows 平台的动态库文件名（探测逻辑跨平台单测用）。
#[cfg(target_os = "linux")]
pub const RUNTIME_LIB_FILE: &str = "libonnxruntime.so";
/// 非 Windows 平台的动态库文件名（探测逻辑跨平台单测用）。
#[cfg(target_os = "macos")]
pub const RUNTIME_LIB_FILE: &str = "libonnxruntime.dylib";

// ── 超参数（对齐 RapidOCR 官方默认，见 ch_ppocr_det/utils.py 与 config.yaml）──
//
// 下面的条目只在 Windows 生产代码与跨平台单测里使用，Linux lib 构建
// （既非 Windows 也非 test）中会被整体移除，避免 dead_code 警告噪音。

/// 检测输入：短边不足此值时等比放大（`limit_type = min`）。
pub const DET_LIMIT_SIDE: u32 = 736;
/// 检测输入边长须为 32 的倍数（网络下采样步长）。
#[cfg(any(target_os = "windows", test))]
const DET_SIZE_MULTIPLE: u32 = 32;
/// 检测输入长边上限（防 4K 大图爆内存，超限按 max 钳制）。
#[cfg(any(target_os = "windows", test))]
const DET_MAX_SIDE: u32 = 2000;
/// DB 二值化阈值（概率图 > 0.3 判为文字，模型输出已含 sigmoid，直接用）。
#[cfg(any(target_os = "windows", test))]
const DET_THRESH: f32 = 0.3;
/// 框保留阈值（框内平均概率 < 0.5 丢弃）。
#[cfg(any(target_os = "windows", test))]
const DET_BOX_THRESH: f32 = 0.5;
/// unclip 外扩系数（`d = 面积 × 系数 / 周长`）。
#[cfg(any(target_os = "windows", test))]
const DET_UNCLIP_RATIO: f32 = 1.6;
/// 连通域短边下限（特征图坐标，官方 `min_size = 3`）。
#[cfg(any(target_os = "windows", test))]
const DET_MIN_SIDE: f32 = 3.0;
/// 单次检测最多保留的候选框数（官方 `max_candidates = 1000`）。
#[cfg(any(target_os = "windows", test))]
const DET_MAX_CANDIDATES: usize = 1000;
/// 识别输入行高（PP-OCR rec 固定 48）。
#[cfg(any(target_os = "windows", test))]
const REC_HEIGHT: u32 = 48;
/// 识别输入行宽上限（超宽行等比压到此宽，防长条爆时间）。
#[cfg(any(target_os = "windows", test))]
const REC_MAX_WIDTH: u32 = 512;
/// 识别输入行宽下限（过窄行保底，避免 0 宽）。
#[cfg(any(target_os = "windows", test))]
const REC_MIN_WIDTH: u32 = 16;

/// RapidOCR 插件引擎（`ort` + PP-OCR，`dir` 指向 `plugins/ocr/`）。
#[derive(Debug, Clone)]
pub struct RapidOcrEngine {
    dir: PathBuf,
}

impl RapidOcrEngine {
    /// 用插件目录构造（目录可不存在，此时 [`OcrEngine::is_available`] 为 false）。
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// 当前指向的插件目录。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 插件文件是否齐全（三个模型文件 + 运行时动态库均为普通文件）。
    pub fn model_files_present(dir: &Path) -> bool {
        [DET_MODEL_FILE, REC_MODEL_FILE, KEYS_FILE, RUNTIME_LIB_FILE]
            .iter()
            .all(|f| dir.join(f).is_file())
    }
}

impl OcrEngine for RapidOcrEngine {
    fn name(&self) -> &'static str {
        "rapidocr"
    }

    fn detect(&self, image: &image::DynamicImage) -> anyhow::Result<Vec<TextRegion>> {
        #[cfg(target_os = "windows")]
        {
            let boxes = windows::detect_boxes(&self.dir, image)?;
            Ok(boxes_to_regions(&boxes, true))
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = image;
            anyhow::bail!("RapidOCR 推理仅支持 Windows（缺插件文件时请用系统 OCR）")
        }
    }

    fn detect_and_recognize(
        &self,
        image: &image::DynamicImage,
    ) -> anyhow::Result<Vec<TextRegion>> {
        #[cfg(target_os = "windows")]
        {
            windows::detect_and_recognize_impl(&self.dir, image)
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = image;
            anyhow::bail!("RapidOCR 推理仅支持 Windows（缺插件文件时请用系统 OCR）")
        }
    }

    fn is_available(&self) -> bool {
        Self::model_files_present(&self.dir)
    }
}

/// 检测框（特征图坐标已回映射到原图，跨平台结构，方便单测）。
#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone)]
struct DetBox {
    /// 外接矩形（原图物理像素坐标）。
    bbox: BBox,
    /// 框内平均概率（0.0~1.0，供 Auto 模式分流）。
    score: f32,
}

/// 检测框转 [`TextRegion`]（`for_crop` 时文字为 `None`，供模式一裁剪用）。
#[cfg(any(target_os = "windows", test))]
fn boxes_to_regions(boxes: &[DetBox], for_crop: bool) -> Vec<TextRegion> {
    // 按阅读顺序排：先按 y 分行（同行容差见 `same_line_band`，对齐官方
    // sorted_boxes 的 10px 分行思想），行内按 x 从左到右。严格 `(y, x)`
    // 排序会把"同行但 y 差几个像素"的框顺序搞反，提取面板看着就是乱序。
    let mut sorted: Vec<&DetBox> = boxes.iter().collect();
    sorted.sort_by_key(|b| (b.bbox.y, b.bbox.x));
    let mut lines: Vec<Vec<&DetBox>> = Vec::new();
    for b in sorted {
        let same_line = lines.last().is_some_and(|last: &Vec<&DetBox>| {
            let first = last[0];
            let band = same_line_band(first.bbox.height);
            b.bbox.y.saturating_sub(first.bbox.y) <= band
        });
        if same_line {
            lines.last_mut().expect("刚判有行").push(b);
        } else {
            lines.push(vec![b]);
        }
    }
    let mut ordered = Vec::with_capacity(boxes.len());
    for mut line in lines {
        line.sort_by_key(|b| b.bbox.x);
        ordered.extend(line);
    }
    let sorted = ordered;
    sorted
        .into_iter()
        .enumerate()
        .map(|(i, b)| TextRegion {
            id: i,
            bbox: b.bbox,
            text: if for_crop { None } else { Some(String::new()) },
            confidence: b.score,
            angle: 0.0,
            est_font_size: TextRegion::estimate_font_size(b.bbox.height),
        })
        .collect()
}

/// 检测输入尺寸：短边不足 [`DET_LIMIT_SIDE`] 则等比放大（`limit_type = min`），
/// 长边超 [`DET_MAX_SIDE`] 则按 max 钳制，最后各边就近取整到 32 的倍数。
/// 返回 `(宽, 高, 宽缩放比, 高缩放比)`（取整导致宽高比有微小差异，分开记）。
#[cfg(any(target_os = "windows", test))]
fn det_resize_dims(w: u32, h: u32) -> (u32, u32, f32, f32) {
    let (w, h) = (w.max(1), h.max(1));
    let mut ratio = 1.0f32;
    if w.min(h) < DET_LIMIT_SIDE {
        let m = w.min(h) as f32;
        ratio = DET_LIMIT_SIDE as f32 / m;
    }
    if w.max(h) as f32 * ratio > DET_MAX_SIDE as f32 {
        ratio = DET_MAX_SIDE as f32 / w.max(h) as f32;
    }
    let round32 = |v: f32| ((v / DET_SIZE_MULTIPLE as f32).round() as u32).max(1) * DET_SIZE_MULTIPLE;
    let (nw, nh) = (round32(w as f32 * ratio), round32(h as f32 * ratio));
    (nw, nh, nw as f32 / w as f32, nh as f32 / h as f32)
}

/// 检测归一化（官方：`(x / 255 - 0.5) / 0.5`），输出 CHW 排布。
#[cfg(any(target_os = "windows", test))]
fn det_normalize(rgb: &image::RgbImage) -> Vec<f32> {
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    let mut out = vec![0.0f32; 3 * w * h];
    for (i, p) in rgb.pixels().enumerate() {
        out[i] = (p[0] as f32 / 255.0 - 0.5) / 0.5;
        out[w * h + i] = (p[1] as f32 / 255.0 - 0.5) / 0.5;
        out[2 * w * h + i] = (p[2] as f32 / 255.0 - 0.5) / 0.5;
    }
    out
}

/// 识别归一化（与检测同一公式），输出 CHW 排布。
#[cfg(any(target_os = "windows", test))]
fn rec_normalize(rgb: &image::RgbImage) -> Vec<f32> {
    det_normalize(rgb)
}

/// 识别输入尺寸：行高固定 [`REC_HEIGHT`]，宽度按比例缩放后钳制到
/// [`REC_MIN_WIDTH`]~[`REC_MAX_WIDTH`]。
#[cfg(any(target_os = "windows", test))]
fn rec_resize_dims(w: u32, h: u32) -> (u32, u32) {
    let (w, h) = (w.max(1), h.max(1));
    let nw = ((REC_HEIGHT as f32 * w as f32 / h as f32).round() as u32)
        .clamp(REC_MIN_WIDTH, REC_MAX_WIDTH);
    (nw, REC_HEIGHT)
}

/// 连通域（BFS 四邻域），返回外接矩形 `(x0, y0, x1, y1)`（右下为开区间）与像素数。
#[cfg(any(target_os = "windows", test))]
fn connected_components(mask: &[bool], w: usize, h: usize) -> Vec<(u32, u32, u32, u32, usize)> {
    let mut seen = vec![false; w * h];
    let mut out = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let idx = y * w + x;
            if !mask[idx] || seen[idx] {
                continue;
            }
            // BFS
            let (mut x0, mut y0, mut x1, mut y1) = (x, y, x + 1, y + 1);
            let mut count = 0usize;
            let mut stack = vec![(x, y)];
            seen[idx] = true;
            while let Some((cx, cy)) = stack.pop() {
                count += 1;
                x0 = x0.min(cx);
                y0 = y0.min(cy);
                x1 = x1.max(cx + 1);
                y1 = y1.max(cy + 1);
                // 四邻域
                if cx > 0 && !seen[cy * w + cx - 1] && mask[cy * w + cx - 1] {
                    seen[cy * w + cx - 1] = true;
                    stack.push((cx - 1, cy));
                }
                if cx + 1 < w && !seen[cy * w + cx + 1] && mask[cy * w + cx + 1] {
                    seen[cy * w + cx + 1] = true;
                    stack.push((cx + 1, cy));
                }
                if cy > 0 && !seen[(cy - 1) * w + cx] && mask[(cy - 1) * w + cx] {
                    seen[(cy - 1) * w + cx] = true;
                    stack.push((cx, cy - 1));
                }
                if cy + 1 < h && !seen[(cy + 1) * w + cx] && mask[(cy + 1) * w + cx] {
                    seen[(cy + 1) * w + cx] = true;
                    stack.push((cx, cy + 1));
                }
            }
            out.push((x0 as u32, y0 as u32, x1 as u32, y1 as u32, count));
        }
    }
    out
}

/// unclip 外扩：`d = 面积 × 系数 / 周长`，矩形每边向外扩 `d`（官方 pyclipper
/// 在旋转矩形上的等价行为；截图文字多为水平，外接矩形足够）。
#[cfg(any(target_os = "windows", test))]
fn unclip_expand(x0: f32, y0: f32, x1: f32, y1: f32) -> (f32, f32, f32, f32) {
    let (w, h) = ((x1 - x0).max(0.0), (y1 - y0).max(0.0));
    let d = if w + h > 0.0 {
        w * h * DET_UNCLIP_RATIO / (2.0 * (w + h))
    } else {
        0.0
    };
    (x0 - d, y0 - d, x1 + d, y1 + d)
}

/// 同行判定带宽（像素）：官方固定 10px；大字场景放宽到半行高，
/// 避免同行被 y 差几像素劈成两行（上限 40px，防跨行误并）。
#[cfg(any(target_os = "windows", test))]
fn same_line_band(height: u32) -> u32 {
    (height / 2).clamp(10, 40)
}

/// 字典文本解析：与官方 `read_character_file` 逐行读取一致——末尾换行
/// 不产生空条目（`str::lines` 语义），其余行原样保留（空格行可能是有效字符）。
/// 注意：`split('\n')` 会多出一个末尾空串，导致类别数差一被模型拒收，此坑已踩过。
#[cfg(any(target_os = "windows", test))]
fn parse_keys(text: &str) -> Vec<String> {
    text.lines().map(|l| l.to_string()).collect()
}

/// 组装解码表：`["blank"] + 文件行 + [" "]`（与官方 `get_character` 一致，
/// 0 号固定为 CTC blank，末尾补空格）。
#[cfg(any(target_os = "windows", test))]
fn build_charset(lines: Vec<String>) -> Vec<String> {
    let mut chars = Vec::with_capacity(lines.len() + 2);
    chars.push("blank".to_string());
    chars.extend(lines);
    chars.push(" ".to_string());
    chars
}

/// CTC 贪心解码（官方 `decode(remove_duplicate=True)`）：逐时刻 argmax，
/// 去连续重复，丢弃 blank(0)，置信度为保留时刻概率均值。
/// `probs` 为行优先 `[时刻 × 类别]`，返回 `(文本, 置信度)`。
#[cfg(any(target_os = "windows", test))]
fn ctc_decode(probs: &[f32], steps: usize, classes: usize, chars: &[String]) -> (String, f32) {
    if steps == 0 || classes == 0 || chars.len() != classes {
        return (String::new(), 0.0);
    }
    let mut text = String::new();
    let mut conf_sum = 0.0f32;
    let mut kept = 0usize;
    let mut prev = usize::MAX;
    for t in 0..steps {
        let row = &probs[t * classes..(t + 1) * classes];
        let (mut best, mut best_p) = (0, row[0]);
        for (c, &p) in row.iter().enumerate().skip(1) {
            if p > best_p {
                best = c;
                best_p = p;
            }
        }
        if best != prev && best != 0 {
            if let Some(ch) = chars.get(best) {
                // "blank" 只出现在 0 号位，此处不会命中；空格正常拼接
                if ch != "blank" {
                    text.push_str(ch);
                    conf_sum += best_p;
                    kept += 1;
                }
            }
        }
        prev = best;
    }
    if kept == 0 {
        return (String::new(), 0.0);
    }
    (text, conf_sum / kept as f32)
}

/// 按检测框裁剪原图（框向外扩 1px 保护笔画，钳制在图内；空框返回 `None`）。
#[cfg(target_os = "windows")]
fn crop_box(rgb: &image::RgbImage, bbox: &BBox) -> Option<image::RgbImage> {
    let (w, h) = (rgb.width(), rgb.height());
    let x0 = bbox.x.saturating_sub(1).min(w);
    let y0 = bbox.y.saturating_sub(1).min(h);
    let x1 = bbox.x.saturating_add(bbox.width).saturating_add(1).min(w);
    let y1 = bbox.y.saturating_add(bbox.height).saturating_add(1).min(h);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(image::imageops::crop_imm(rgb, x0, y0, x1 - x0, y1 - y0).to_image())
}

/// DB 后处理入口（特征图展平 + 输出形状 → 框列表；形状不对直接空结果）。
#[cfg(any(target_os = "windows", test))]
fn postprocess(
    flat: &[f32],
    dims: &[i64],
    ow: u32,
    oh: u32,
) -> Vec<DetBox> {
    let (ph, pw) = match dims {
        [1, 1, h, w] => (*h as usize, *w as usize),
        _ => return Vec::new(),
    };
    postprocess_map(flat, pw, ph, ow, oh)
}

/// DB 后处理本体：二值化 → 2x2 膨胀 → 连通域 → 计分过滤 → unclip →
/// 回映射原图坐标。
///
/// 关键：检测模型输出是带步长的特征图（如 DB 默认步长 4，即概率图
/// 只有输入的 1/4 大），必须按**实测特征图尺寸**换算（`原图 = 特征图 ×
/// 原图宽 / 特征图宽`，与官方 `box / width * dest_width` 一致）。
/// 早前误按"输入图→原图"缩放比换算，框被整体缩小数倍挤在左上——
/// 翻译贴错位置的根因（2026-09-06 实机复现）。
#[cfg(any(target_os = "windows", test))]
fn postprocess_map(
    prob: &[f32],
    pw: usize,
    ph: usize,
    ow: u32,
    oh: u32,
) -> Vec<DetBox> {
    // 实测换算比（不假设步长，v4/v6 通用）
    let kx = ow as f32 / pw.max(1) as f32;
    let ky = oh as f32 / ph.max(1) as f32;
    if prob.len() < pw * ph || pw == 0 || ph == 0 {
        return Vec::new();
    }
    // 二值化 + 2x2 膨胀（官方 use_dilation 默认开）
    let mut mask = vec![false; pw * ph];
    for y in 0..ph {
        for x in 0..pw {
            if prob[y * pw + x] > DET_THRESH {
                mask[y * pw + x] = true;
                // 向右/向下各扩 1px（2x2 全 1 核的等价行为）
                if x + 1 < pw {
                    mask[y * pw + x + 1] = true;
                }
                if y + 1 < ph {
                    mask[(y + 1) * pw + x] = true;
                }
                if x + 1 < pw && y + 1 < ph {
                    mask[(y + 1) * pw + x + 1] = true;
                }
            }
        }
    }
    let comps = connected_components(&mask, pw, ph);
    let mut out = Vec::new();
    for (x0, y0, x1, y1, _) in comps.into_iter().take(DET_MAX_CANDIDATES) {
        let (fw, fh) = ((x1 - x0) as f32, (y1 - y0) as f32);
        if fw.min(fh) < DET_MIN_SIDE {
            continue;
        }
        // 计分：框内平均概率（官方 box_score_fast 的矩形近似）
        let (mut sum, mut n) = (0.0f64, 0usize);
        for y in y0..y1 {
            for x in x0..x1 {
                sum += prob[(y as usize) * pw + x as usize] as f64;
                n += 1;
            }
        }
        if n == 0 {
            continue;
        }
        let score = (sum / n as f64) as f32;
        if score < DET_BOX_THRESH {
            continue;
        }
        // unclip 外扩后回映射原图坐标（特征图→原图实测换算）
        let (ex0, ey0, ex1, ey1) =
            unclip_expand(x0 as f32, y0 as f32, x1 as f32, y1 as f32);
        let ox0 = (ex0 * kx).round().clamp(0.0, ow as f32) as u32;
        let oy0 = (ey0 * ky).round().clamp(0.0, oh as f32) as u32;
        let ox1 = (ex1 * kx).round().clamp(0.0, ow as f32) as u32;
        let oy1 = (ey1 * ky).round().clamp(0.0, oh as f32) as u32;
        if ox1 <= ox0 || oy1 <= oy0 {
            continue;
        }
        // 过滤碎框（官方 filter：两边 > 3px）
        if ox1 - ox0 <= 3 || oy1 - oy0 <= 3 {
            continue;
        }
        out.push(DetBox {
            bbox: BBox { x: ox0, y: oy0, width: ox1 - ox0, height: oy1 - oy0 },
            score,
        });
    }
    out
}

/// Windows 推理实现（`ort` 会话 + 前后处理组装）。
#[cfg(target_os = "windows")]
pub(super) mod windows {
    use std::path::Path;
    use std::sync::{Mutex, OnceLock};

    use super::{
        build_charset, crop_box, ctc_decode, det_normalize, det_resize_dims, parse_keys,
        postprocess, rec_normalize, rec_resize_dims, DET_MODEL_FILE, KEYS_FILE,
        REC_MODEL_FILE, RUNTIME_LIB_FILE,
    };

    use super::{DetBox, TextRegion};

    /// `ort::Error` 含裸指针、不是 `Send + Sync`，不能直接 `?` 进 anyhow，
    /// 统一经 Display 转字符串（保留原文，方便定位）。
    fn oe(context: &str, e: impl std::fmt::Display) -> anyhow::Error {
        anyhow::anyhow!("{context}：{e}")
    }

    /// 已加载的会话（`Session::run` 需 `&mut`，推理时整组加锁串行）。
    pub(super) struct Loaded {
        det: Mutex<ort::session::Session>,
        rec: Mutex<ort::session::Session>,
        chars: Vec<String>,
    }

    /// 进程内全局缓存（`Some(插件目录, 会话)`；目录变化时重载）。
    static CACHE: OnceLock<Mutex<Option<(std::path::PathBuf, Loaded)>>> = OnceLock::new();
    /// 环境初始化结果缓存（`init_from` 只能成功一次，失败也记住、提示重装 DLL）。
    static ENV: OnceLock<Result<(), String>> = OnceLock::new();

    /// 拿全局会话（未加载/目录变化时重载；调用期间持有全局锁做推理）。
    pub(super) fn with_loaded<R>(
        dir: &Path,
        f: impl FnOnce(&Loaded) -> anyhow::Result<R>,
    ) -> anyhow::Result<R> {
        let cache = CACHE.get_or_init(|| Mutex::new(None));
        let mut guard = cache
            .lock()
            .map_err(|_| anyhow::anyhow!("OCR 会话锁异常（中毒），请重启程序"))?;
        let need_load = guard.as_ref().is_none_or(|(d, _)| d != dir);
        if need_load {
            *guard = Some((dir.to_path_buf(), load(dir)?));
        }
        let loaded = &guard.as_ref().expect("刚写入必有值").1;
        f(loaded)
    }

    /// 环境 + 双会话 + 字典一次装好（任一步失败直接 bail，上层退回系统 OCR）。
    fn load(dir: &Path) -> anyhow::Result<Loaded> {
        let det_path = dir.join(DET_MODEL_FILE);
        let rec_path = dir.join(REC_MODEL_FILE);
        let keys_path = dir.join(KEYS_FILE);
        let dll_path = dir.join(RUNTIME_LIB_FILE);
        for p in [&det_path, &rec_path, &keys_path, &dll_path] {
            if !p.is_file() {
                anyhow::bail!("OCR 插件缺文件：{}（四文件齐全才可用）", p.display());
            }
        }
        // 环境初始化（进程一次；DLL 对不上会在这里失败）
        ENV.get_or_init(|| {
            ort::init_from(&dll_path)
                .map(|b| {
                    b.commit();
                })
                .map_err(|e| format!("{e:#}"))
        })
        .as_ref()
        .map_err(|e| {
            anyhow::anyhow!("onnxruntime.dll 加载失败（可能与 ort 版本不配对，需 ORT 1.28）：{e}")
        })?;
        let det = ort::session::Session::builder()
            .map_err(|e| oe("检测会话构造失败", e))?
            .with_intra_threads(4)
            .map_err(|e| oe("检测线程数设置失败", e))?
            .commit_from_file(&det_path)
            .map_err(|e| anyhow::anyhow!("检测模型加载失败（{}）：{e}", det_path.display()))?;
        let rec = ort::session::Session::builder()
            .map_err(|e| oe("识别会话构造失败", e))?
            .with_intra_threads(4)
            .map_err(|e| oe("识别线程数设置失败", e))?
            .commit_from_file(&rec_path)
            .map_err(|e| anyhow::anyhow!("识别模型加载失败（{}）：{e}", rec_path.display()))?;
        let text = std::fs::read(&keys_path)
            .map_err(|e| anyhow::anyhow!("字典读取失败（{}）：{e:#}", keys_path.display()))?;
        let text = String::from_utf8(text)
            .map_err(|_| anyhow::anyhow!("字典不是合法 UTF-8（{}），请用配套 keys.txt", keys_path.display()))?;
        let chars = build_charset(parse_keys(&text));
        Ok(Loaded { det: Mutex::new(det), rec: Mutex::new(rec), chars })
    }

    impl Loaded {
        /// 单行识别：resize → 归一化 → 推理 → CTC 解码（返回空串表示无有效文字）。
        fn recognize(&self, crop: &image::RgbImage) -> anyhow::Result<(String, f32)> {
            let (nw, nh) = rec_resize_dims(crop.width(), crop.height());
            let resized = image::imageops::resize(
                crop,
                nw,
                nh,
                image::imageops::FilterType::Triangle,
            );
            let data = rec_normalize(&resized);
            let input =
                ndarray::Array4::from_shape_vec((1, 3, nh as usize, nw as usize), data)
                    .map_err(|e| anyhow::anyhow!("识别输入组包失败：{e}"))?;
            let input_ref = ort::value::TensorRef::from_array_view(&input)
                .map_err(|e| oe("识别输入组包失败", e))?;
            // 先绑定 guard（`run` 返回借用会话的输出，不能挂在临时锁上）
            let mut rec = self
                .rec
                .lock()
                .map_err(|_| anyhow::anyhow!("OCR 会话锁异常（中毒），请重启程序"))?;
            let outputs = rec
                .run(ort::inputs![input_ref])
                .map_err(|e| oe("识别推理失败", e))?;
            let (shape, data) = outputs[0]
                .try_extract_tensor::<f32>()
                .map_err(|e| oe("识别输出解析失败", e))?;
            // 输出可能是 [N, T, C] 或 [T, C]，统一成 (时刻数, 类别数)
            let dims: Vec<i64> = shape.iter().copied().collect();
            let (steps, classes, flat) = match dims.as_slice() {
                [_, t, c] => (*t as usize, *c as usize, data),
                [t, c] => (*t as usize, *c as usize, data),
                _ => anyhow::bail!("识别模型输出形状异常（{dims:?}），可能模型文件不对"),
            };
            if classes != self.chars.len() {
                anyhow::bail!(
                    "keys.txt 与识别模型不配套（字典 {} 类 vs 模型 {} 类），请用同套文件，否则会乱码",
                    self.chars.len(),
                    classes
                );
            }
            if data.len() < steps * classes {
                anyhow::bail!("识别输出数据长度异常（{} < {}），跳过该行", data.len(), steps * classes);
            }
            Ok(ctc_decode(flat, steps, classes, &self.chars))
        }
    }

    /// 检测框（仅检测，供 `detect` 与 `detect_and_recognize` 共用）。
    pub(super) fn detect_boxes(
        dir: &Path,
        image: &image::DynamicImage,
    ) -> anyhow::Result<Vec<DetBox>> {
        let rgb = image.to_rgb8();
        let (w, h) = (rgb.width(), rgb.height());
        if w == 0 || h == 0 {
            anyhow::bail!("空图像无法识别");
        }
        let (nw, nh, _, _) = det_resize_dims(w, h);
        let resized =
            image::imageops::resize(&rgb, nw, nh, image::imageops::FilterType::Triangle);
        let data = det_normalize(&resized);
        let probs = with_loaded(dir, |loaded| {
            let input =
                ndarray::Array4::from_shape_vec((1, 3, nh as usize, nw as usize), data)
                    .map_err(|e| anyhow::anyhow!("检测输入组包失败：{e}"))?;
            let input_ref = ort::value::TensorRef::from_array_view(&input)
                .map_err(|e| oe("检测输入组包失败", e))?;
            // 先绑定 guard（`run` 返回借用会话的输出，不能挂在临时锁上）
            let mut det = loaded
                .det
                .lock()
                .map_err(|_| anyhow::anyhow!("OCR 会话锁异常（中毒），请重启程序"))?;
            let outputs = det
                .run(ort::inputs![input_ref])
                .map_err(|e| oe("检测推理失败", e))?;
            let (shape, data) = outputs[0]
                .try_extract_tensor::<f32>()
                .map_err(|e| oe("检测输出解析失败", e))?;
            let dims: Vec<i64> = shape.iter().copied().collect();
            // 输出应为 [1, 1, H, W]，展平成概率图
            let flat: Vec<f32> = match dims.as_slice() {
                [1, 1, ph, pw] => data[..(*ph as usize * *pw as usize).min(data.len())].to_vec(),
                _ => anyhow::bail!("检测模型输出形状异常（{dims:?}），可能模型文件不对"),
            };
            Ok((flat, dims))
        })?;
        Ok(postprocess(&probs.0, &probs.1, w, h))
    }

    /// 检测 + 识别一体（检测与逐行识别在同一次全局锁内完成，省一次加锁）。
    pub(super) fn detect_and_recognize_impl(
        dir: &Path,
        image: &image::DynamicImage,
    ) -> anyhow::Result<Vec<TextRegion>> {
        let boxes = detect_boxes(dir, image)?;
        let rgb = image.to_rgb8();
        with_loaded(dir, |loaded| {
            let mut regions = Vec::with_capacity(boxes.len());
            for (i, b) in boxes.iter().enumerate() {
                let Some(crop) = crop_box(&rgb, &b.bbox) else {
                    continue;
                };
                let (text, conf) = loaded.recognize(&crop)?;
                if text.is_empty() {
                    continue;
                }
                regions.push(TextRegion {
                    id: i,
                    bbox: b.bbox,
                    text: Some(text),
                    confidence: conf,
                    angle: 0.0,
                    est_font_size: TextRegion::estimate_font_size(b.bbox.height),
                });
            }
            Ok(regions)
        })
    }

    #[cfg(test)]
    pub(super) mod tests {
        // Windows-only 推理无法在 WSL 跑；纯逻辑（见外层 tests）跨平台可测。
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_plugin_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("prismsnap_ocr_{name}"))
    }

    fn clean(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_dir_is_unavailable() {
        let dir = temp_plugin_dir("missing");
        clean(&dir);
        assert!(!RapidOcrEngine::model_files_present(&dir));
        assert!(!RapidOcrEngine::new(dir).is_available());
    }

    #[test]
    fn complete_plugin_dir_is_available() {
        let dir = temp_plugin_dir("complete");
        clean(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in [DET_MODEL_FILE, REC_MODEL_FILE, KEYS_FILE, RUNTIME_LIB_FILE] {
            std::fs::write(dir.join(f), b"dummy").unwrap();
        }
        assert!(RapidOcrEngine::new(dir.clone()).is_available());
        clean(&dir);
    }

    #[test]
    fn partial_plugin_dir_is_unavailable() {
        let dir = temp_plugin_dir("partial");
        clean(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 缺 onnxruntime 动态库即视为不可用（运行时加载不到会崩，不如提前降级）
        for f in [DET_MODEL_FILE, REC_MODEL_FILE, KEYS_FILE] {
            std::fs::write(dir.join(f), b"dummy").unwrap();
        }
        assert!(!RapidOcrEngine::new(dir.clone()).is_available());
        clean(&dir);
    }

    #[test]
    fn det_resize_upscales_short_side() {
        // 400x300：短边 300 → 放大到 736（min 语义），再取整到 32 倍数
        let (nw, nh, sx, sy) = det_resize_dims(400, 300);
        assert_eq!((nw, nh), (992, 736));
        assert!((sx - 992.0 / 400.0).abs() < 1e-6);
        assert!((sy - 736.0 / 300.0).abs() < 1e-6);
    }

    #[test]
    fn det_resize_keeps_large_image() {
        // 1920x1080：短边已超 736，不缩放（就近 32 取整后不变）
        let (nw, nh, _, _) = det_resize_dims(1920, 1080);
        assert_eq!((nw, nh), (1920, 1088));
    }

    #[test]
    fn det_normalize_matches_official_formula() {
        // (x / 255 - 0.5) / 0.5：纯黑 → -1，纯白 → 1，中灰 ≈ 0
        let img = image::RgbImage::from_pixel(1, 1, image::Rgb([128, 128, 128]));
        let v = det_normalize(&img);
        assert_eq!(v.len(), 3);
        let expect = (128.0 / 255.0 - 0.5) / 0.5;
        for c in v {
            assert!((c - expect).abs() < 1e-6);
        }
    }

    #[test]
    fn components_finds_two_boxes() {
        // 6x4 图，左右各一块
        let (w, h) = (6, 4);
        let mut mask = vec![false; w * h];
        for y in 0..h {
            for x in [0, 1, 4, 5] {
                mask[y * w + x] = true;
            }
        }
        let mut comps = connected_components(&mask, w, h);
        comps.sort();
        assert_eq!(comps.len(), 2);
        assert_eq!(comps[0], (0, 0, 2, 4, 8));
        assert_eq!(comps[1], (4, 0, 6, 4, 8));
    }

    #[test]
    fn unclip_expands_by_area_over_perimeter() {
        // 10x10 方块：d = 100×1.6/40 = 4，每边外扩 4
        let (x0, y0, x1, y1) = unclip_expand(0.0, 0.0, 10.0, 10.0);
        assert!((x0 + 4.0).abs() < 1e-5);
        assert!((y0 + 4.0).abs() < 1e-5);
        assert!((x1 - 14.0).abs() < 1e-5);
        assert!((y1 - 14.0).abs() < 1e-5);
    }

    #[test]
    fn keys_parse_keeps_blank_first_line() {
        // 首行空串原样保留；末尾换行不产生空条目（与官方 readlines 一致，
        // 否则类别数差一，实机报过"字典 6626 类 vs 模型 6625 类"）
        let lines = parse_keys("\n疗\n绚\r\n");
        assert_eq!(lines, vec!["".to_string(), "疗".to_string(), "绚".to_string()]);
        let chars = build_charset(lines);
        assert_eq!(chars[0], "blank");
        assert_eq!(chars[1], "");
        assert_eq!(chars[2], "疗");
        assert_eq!(chars.last().unwrap(), " ");
    }

    #[test]
    fn ctc_decode_dedups_and_drops_blank() {
        // 字典：[blank, a, b]；时刻：a a blank b b → "ab"，置信度为保留概率均值
        let chars = vec!["blank".into(), "a".into(), "b".into()];
        let probs = vec![
            0.1, 0.8, 0.1, // a(0.8)
            0.1, 0.7, 0.2, // a 重复，去掉
            0.9, 0.05, 0.05, // blank，去掉
            0.1, 0.2, 0.7, // b(0.7)
            0.2, 0.2, 0.6, // b 重复，去掉
        ];
        let (text, conf) = ctc_decode(&probs, 5, 3, &chars);
        assert_eq!(text, "ab");
        assert!((conf - 0.75).abs() < 1e-6);
    }

    #[test]
    fn ctc_decode_rejects_charset_mismatch() {
        // 类别数与字典长度不一致 → 空串（上层按无文字跳过，不造乱码）
        let chars = vec!["blank".into(), "a".into()];
        let (text, conf) = ctc_decode(&[0.5, 0.5, 0.5], 1, 3, &chars);
        assert!(text.is_empty());
        assert_eq!(conf, 0.0);
    }

    #[test]
    fn postprocess_uses_measured_stride() {
        // 步长 4 回归：16x16 特征图 ← 64x64 原图，热区 (4..8, 4..8)；
        // 膨胀 → (4,4,9,9)；unclip d=2 → (2,2,11,11)；×4 → (8,8,44,44)。
        // 早前误按输入缩放比换算会给出约 9px 的小框挤在左上（实机 bug）。
        let (pw, ph) = (16usize, 16usize);
        let mut prob = vec![0.0f32; pw * ph];
        for y in 4..8 {
            for x in 4..8 {
                prob[y * pw + x] = 0.9;
            }
        }
        let boxes = postprocess_map(&prob, pw, ph, 64, 64);
        assert_eq!(boxes.len(), 1);
        assert_eq!(
            boxes[0].bbox,
            BBox { x: 8, y: 8, width: 36, height: 36 }
        );
        assert!(boxes[0].score > 0.5);
    }

    #[test]
    fn postprocess_entry_checks_shape() {
        let prob = vec![0.9f32; 16 * 16];
        assert_eq!(postprocess(&prob, &[1, 1, 16, 16], 64, 64).len(), 1);
        assert!(postprocess(&prob, &[1, 3, 16, 16], 64, 64).is_empty());
        assert!(postprocess(&[], &[1, 1, 16, 16], 64, 64).is_empty());
    }

    #[test]
    fn boxes_sort_reading_order() {
        let mk = |x, y| DetBox {
            bbox: BBox { x, y, width: 10, height: 10 },
            score: 0.9,
        };
        let regions = boxes_to_regions(&[mk(50, 0), mk(0, 0), mk(0, 30)], true);
        assert_eq!((regions[0].bbox.x, regions[0].bbox.y), (0, 0));
        assert_eq!((regions[1].bbox.x, regions[1].bbox.y), (50, 0));
        assert_eq!((regions[2].bbox.x, regions[2].bbox.y), (0, 30));
        assert!(regions.iter().all(|r| r.text.is_none()));
    }

    #[test]
    fn boxes_same_line_sorted_by_x_despite_y_jitter() {
        // 同行 y 差几像素（d/11 vs a/8）：必须按 x 排，否则提取面板乱序
        let mk = |x, y| DetBox {
            bbox: BBox { x, y, width: 20, height: 16 },
            score: 0.9,
        };
        let regions = boxes_to_regions(&[mk(100, 11), mk(0, 8)], true);
        assert_eq!(regions[0].bbox.x, 0);
        assert_eq!(regions[1].bbox.x, 100);
    }

    #[test]
    fn boxes_different_lines_not_merged() {
        // y 差 30（band 上限 40？半行高 8→band 10）：分成两行
        let mk = |x, y| DetBox {
            bbox: BBox { x, y, width: 20, height: 16 },
            score: 0.9,
        };
        let regions = boxes_to_regions(&[mk(100, 40), mk(0, 8)], true);
        assert_eq!((regions[0].bbox.x, regions[0].bbox.y), (0, 8));
        assert_eq!((regions[1].bbox.x, regions[1].bbox.y), (100, 40));
        // 大字：半行高放宽（高 60 → band 30），y 差 25 仍同行按 x
        let big = |x, y| DetBox {
            bbox: BBox { x, y, width: 40, height: 60 },
            score: 0.9,
        };
        let regions = boxes_to_regions(&[big(200, 25), big(0, 0)], true);
        assert_eq!(regions[0].bbox.x, 0);
        assert_eq!(regions[1].bbox.x, 200);
    }
}
