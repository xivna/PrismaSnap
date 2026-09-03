//! 区域合并（行 → 语义块版面分析，跨平台纯函数，见 AGENTS.md 3.8 节）。
//!
//! OCR 按"行"输出结果，直接逐行翻译会割裂语义（一句话被换行拆成两行）。
//! 本模块按几何邻近关系把相邻行合并为语义块，供翻译调度层消费。
//! 默认按段落合并（通顺）；阈值常量已按常规 UI 行距调过，高级"按行保真"选项预留。

use crate::ocr::{BBox, TextBlock, TextRegion};

/// 垂直间距阈值系数：相邻行间距 `<` 行高 × 此系数即视为同段。
///
/// 常规 UI 行距下行间隙约 0.25 倍字高、段间距通常 ≥0.5 倍字高，取 0.6 兼顾
/// "同段行必合并"与"跨段落不断错合并"（启发式，可调）。
pub const GAP_FACTOR: f32 = 0.6;

/// 水平重叠率下限（交集宽度 / 较窄者宽度），低于此值视为不同栏，不合并。
pub const OVERLAP_RATIO: f32 = 0.3;

/// 字号相近容差（差值 / 较大者），超出视为标题/正文混排，不合并。
pub const FONT_SIZE_TOLERANCE: f32 = 0.3;

/// 文本拼接分隔符（保留换行信息，LLM 可正确处理；CJK/拉丁混排无需空格启发式）。
pub const JOIN_SEPARATOR: &str = "\n";

/// 把 OCR 行结果合并为语义块（按 y、x 排序后贪心链式合并）。
///
/// - 零面积行直接丢弃（防除零）；
/// - 子区域任一 `text` 为 `None`（仅检测模式）时 `merged_text` 为 `None`，
///   包围盒仍正常合并（供模式一裁剪用）；
/// - 空输入返回空输出。
pub fn merge_regions_into_blocks(mut regions: Vec<TextRegion>) -> Vec<TextBlock> {
    regions.retain(|r| r.bbox.width > 0 && r.bbox.height > 0);
    regions.sort_by_key(|r| (r.bbox.y, r.bbox.x));

    let mut blocks: Vec<TextBlock> = Vec::new();
    for region in regions {
        let merge_into_last = blocks
            .last()
            .and_then(|b| b.regions.last())
            .is_some_and(|last| should_merge(last, &region));
        if merge_into_last {
            let block = blocks.last_mut().expect("刚检查过非空");
            block.bbox = block.bbox.union(region.bbox);
            block.regions.push(region);
            block.merged_text = join_texts(&block.regions);
        } else {
            let text = region.text.clone();
            blocks.push(TextBlock {
                bbox: region.bbox,
                regions: vec![region],
                merged_text: text,
            });
        }
    }
    blocks
}

/// 相邻两行是否应合并（调用方保证 `a` 在上、`b` 在下，即已按 y 排序）。
fn should_merge(a: &TextRegion, b: &TextRegion) -> bool {
    // 1. 垂直间距：下行顶 - 上行底（上行包住下行时 saturating 归零，即重叠行必过此关）。
    let gap = b.bbox.y.saturating_sub(a.bbox.y.saturating_add(a.bbox.height));
    let line_h = a.bbox.height.max(b.bbox.height);
    if line_h == 0 || gap as f32 >= line_h as f32 * GAP_FACTOR {
        return false;
    }
    // 2. 水平重叠：交集宽 / 较窄者宽（不同栏左右并列时不过）。
    let left = a.bbox.x.max(b.bbox.x);
    let right = (a.bbox.x.saturating_add(a.bbox.width))
        .min(b.bbox.x.saturating_add(b.bbox.width));
    let inter = right.saturating_sub(left);
    let min_w = a.bbox.width.min(b.bbox.width);
    if min_w == 0 || inter as f32 / min_w as f32 <= OVERLAP_RATIO {
        return false;
    }
    // 3. 字号相近（标题/正文混排不过）。
    let max_font = a.est_font_size.max(b.est_font_size);
    if max_font == 0 {
        return true;
    }
    let diff = a.est_font_size.abs_diff(b.est_font_size);
    diff as f32 <= max_font as f32 * FONT_SIZE_TOLERANCE
}

/// 拼接子区域文本（任一 `None` 则整体 `None`）。
fn join_texts(regions: &[TextRegion]) -> Option<String> {
    let mut out = String::new();
    for (i, r) in regions.iter().enumerate() {
        let text = r.text.as_ref()?;
        if i > 0 {
            out.push_str(JOIN_SEPARATOR);
        }
        out.push_str(text);
    }
    Some(out)
}

/// 外接矩形求并（[`BBox::union`] 的多元素版本，空输入返回零矩形）。
pub fn outer_bbox(boxes: &[BBox]) -> BBox {
    boxes
        .iter()
        .copied()
        .reduce(|a, b| a.union(b))
        .unwrap_or(BBox { x: 0, y: 0, width: 0, height: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: usize, x: u32, y: u32, w: u32, h: u32, text: &str) -> TextRegion {
        TextRegion {
            id,
            bbox: BBox { x, y, width: w, height: h },
            text: Some(text.to_owned()),
            confidence: 0.9,
            angle: 0.0,
            est_font_size: h,
        }
    }

    #[test]
    fn close_lines_merge_into_one_block() {
        // 行高 20，行间距 5（< 20*0.6=12），水平完全重叠 → 合并
        let blocks = merge_regions_into_blocks(vec![
            line(0, 10, 10, 200, 20, "第一行"),
            line(1, 10, 35, 200, 20, "第二行"),
        ]);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].regions.len(), 2);
        assert_eq!(blocks[0].merged_text.as_deref(), Some("第一行\n第二行"));
        assert_eq!(
            blocks[0].bbox,
            BBox { x: 10, y: 10, width: 200, height: 45 }
        );
    }

    #[test]
    fn far_lines_stay_separate() {
        // 行间距 40（> 12）→ 不合并
        let blocks = merge_regions_into_blocks(vec![
            line(0, 10, 10, 200, 20, "上段"),
            line(1, 10, 70, 200, 20, "下段"),
        ]);
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn side_by_side_columns_stay_separate() {
        // 同一 y，不同栏（水平无重叠）→ 不合并
        let blocks = merge_regions_into_blocks(vec![
            line(0, 10, 10, 100, 20, "左栏"),
            line(1, 200, 10, 100, 20, "右栏"),
        ]);
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn different_font_sizes_stay_separate() {
        // 标题（40px）与正文（16px）即使相邻也不合并
        let mut title = line(0, 10, 10, 200, 40, "标题");
        title.est_font_size = 40;
        let mut body = line(1, 10, 55, 200, 16, "正文");
        body.est_font_size = 16;
        let blocks = merge_regions_into_blocks(vec![title, body]);
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn unsorted_input_is_sorted_by_position() {
        let blocks = merge_regions_into_blocks(vec![
            line(1, 10, 35, 200, 20, "第二行"),
            line(0, 10, 10, 200, 20, "第一行"),
        ]);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].merged_text.as_deref(), Some("第一行\n第二行"));
    }

    #[test]
    fn detect_only_regions_merge_bbox_but_no_text() {
        // 模式一仅检测路径：text 为 None，包围盒仍合并供裁剪用
        let mut a = line(0, 10, 10, 200, 20, "x");
        a.text = None;
        let mut b = line(1, 10, 35, 200, 20, "y");
        b.text = None;
        let blocks = merge_regions_into_blocks(vec![a, b]);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].merged_text, None);
        assert_eq!(blocks[0].bbox.height, 45);
    }

    #[test]
    fn empty_and_degenerate_inputs() {
        assert!(merge_regions_into_blocks(vec![]).is_empty());
        // 零面积行丢弃
        let zero = TextRegion {
            id: 0,
            bbox: BBox { x: 5, y: 5, width: 0, height: 0 },
            text: Some(String::from("x")),
            confidence: 1.0,
            angle: 0.0,
            est_font_size: 8,
        };
        assert!(merge_regions_into_blocks(vec![zero]).is_empty());
        assert_eq!(
            outer_bbox(&[]),
            BBox { x: 0, y: 0, width: 0, height: 0 }
        );
    }
}
