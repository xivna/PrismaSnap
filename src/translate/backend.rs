//! 翻译双后端：Prompt 模板 + 宽松 JSON 解析（跨平台纯逻辑，见 AGENTS.md 3.8 节）。
//!
//! - 模式一（裁剪 → 多模态）：识别 + 翻译一体，输入裁剪小图；
//! - 模式二（OCR → 纯文本）：输入 `id + text` 数组，输出等长 `id + translation`；
//! - 解析宽松化：先去 markdown 代码围栏再 `serde_json` 严格解析，失败则用
//!   无依赖手写扫描器兜底提字段，再失败则丢弃该条（不影响其余区域）。
//! - 异步 `TranslationBackend` trait 与 HTTP 组包在 `llm/client.rs` 步骤落地，
//!   本模块只负责可单测的纯函数部分。

use serde::Deserialize;

/// 模式二单条译文（`id` 对齐，防 LLM 合并/漏译错位）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TextTranslation {
    pub id: usize,
    pub translation: String,
}

/// 模式一单图识别 + 翻译结果。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ImageTranslation {
    #[serde(default)]
    pub original: String,
    #[serde(default)]
    pub translation: String,
}

/// 模式二 Prompt：用 `id` 显式对齐，要求条目数量/顺序/`id` 不变、不合并拆分。
pub fn build_text_prompt(inputs: &[(usize, &str)], target_lang: &str) -> String {
    let mut items = String::new();
    for (i, (id, text)) in inputs.iter().enumerate() {
        if i > 0 {
            items.push_str(",\n");
        }
        items.push_str(&format!(
            "{{\"id\": {id}, \"text\": {text}}}",
            text = json_string(text)
        ));
    }
    format!(
        "你是专业翻译。请将下面 JSON 数组中每一项的 \"text\" 字段翻译为{target_lang}，\n\
         并保持 \"id\" 不变、条目数量不变、顺序不变，不要合并或拆分条目。\n\
         严格按以下 JSON 格式输出，不要输出任何解释性文字：\n\
         [{{\"id\": 1, \"translation\": \"...\"}}, {{\"id\": 2, \"translation\": \"...\"}}]\n\
         \n\
         输入：\n\
         [{items}]"
    )
}

/// 模式一 Prompt：单张裁剪图，识别 + 翻译一体。
pub fn build_multimodal_prompt(target_lang: &str) -> String {
    format!(
        "你是专业的图像文字识别与翻译助手。这是一张截图局部区域的图片，\n\
         其中包含一行或多行文字。请完成：\n\
         1. 按阅读顺序识别图片中的所有文字；\n\
         2. 将识别结果翻译为{target_lang}；\n\
         3. 严格按以下 JSON 格式输出，不要输出任何多余内容：\n\
         {{\"original\": \"识别出的原文\", \"translation\": \"对应译文\"}}\n\
         若图片中没有可识别的文字，输出 {{\"original\": \"\", \"translation\": \"\"}}"
    )
}

/// 模式一 Prompt：多张裁剪图一次请求（要求按序返回数组 + 序号，防错位）。
pub fn build_multimodal_batch_prompt(target_lang: &str, image_count: usize) -> String {
    format!(
        "你是专业的图像文字识别与翻译助手。下面有 {image_count} 张截图局部区域的图片，\n\
         每张包含一行或多行文字。请对每张图完成：\n\
         1. 按阅读顺序识别图片中的所有文字；\n\
         2. 将识别结果翻译为{target_lang}；\n\
         3. 严格按输入图片顺序返回一个 JSON 数组，每项带序号，不要输出任何多余内容：\n\
         [{{\"index\": 0, \"original\": \"原文\", \"translation\": \"译文\"}}, ...]\n\
         若某张图没有可识别的文字，该项输出 {{\"index\": N, \"original\": \"\", \"translation\": \"\"}}"
    )
}

/// 解析模式二返回（数组，`id` 对齐； salvaged 条目按出现顺序返回）。
pub fn parse_text_response(body: &str) -> Vec<TextTranslation> {
    let clean = strip_fences(body);
    // 1. 严格解析。
    if let Ok(items) = serde_json::from_str::<Vec<TextTranslation>>(&clean) {
        return items;
    }
    // 2. 兜底：扫描所有 {"id": N, "translation": "..."} 对。
    scan_id_translations(&clean)
}

/// 解析模式一单图返回。
pub fn parse_multimodal_single(body: &str) -> Option<ImageTranslation> {
    let clean = strip_fences(body);
    if let Ok(item) = serde_json::from_str::<ImageTranslation>(&clean) {
        return Some(item);
    }
    scan_original_translation(&clean)
        .into_iter()
        .next()
        .map(|(original, translation)| ImageTranslation { original, translation })
}

/// 解析模式一多图返回（按数组顺序；严格失败时按扫描顺序兜底）。
pub fn parse_multimodal_batch(body: &str) -> Vec<ImageTranslation> {
    let clean = strip_fences(body);
    if let Ok(items) = serde_json::from_str::<Vec<ImageTranslation>>(&clean) {
        return items;
    }
    // 带 index 的批量格式也先尝试严格解析（忽略 index，只取内容）。
    #[derive(Deserialize)]
    struct Indexed {
        #[allow(dead_code)]
        index: usize,
        #[serde(default)]
        original: String,
        #[serde(default)]
        pub translation: String,
    }
    if let Ok(items) = serde_json::from_str::<Vec<Indexed>>(&clean) {
        return items
            .into_iter()
            .map(|i| ImageTranslation { original: i.original, translation: i.translation })
            .collect();
    }
    scan_original_translation(&clean)
        .into_iter()
        .map(|(original, translation)| ImageTranslation { original, translation })
        .collect()
}

/// 翻译后端抽象（统一封装两种模式，`Send + Sync` 可跨线程调度）。
///
/// - 模式二（纯文本）：`translate_text`，输入文本数组，输出等长译文数组
///   （缺失条目由实现方用原文兜底，保证对齐不丢内容）；
/// - 模式一（多模态）：`recognize_and_translate_image`，输入裁剪小图，
///   输出（原文， 译文）对（无法识别的图返回空字符串对，管线跳过不覆盖）。
/// - 不支持的方向直接返回错误（管线保证不调用到，例如文本后端永远只收纯文本）。
#[async_trait::async_trait]
pub trait TranslationBackend: Send + Sync {
    /// 模式二：纯文本翻译。
    async fn translate_text(
        &self,
        texts: &[String],
        target_lang: &str,
    ) -> anyhow::Result<Vec<String>>;

    /// 模式一：裁剪图识别 + 翻译一体。
    async fn recognize_and_translate_image(
        &self,
        crops: &[image::DynamicImage],
        target_lang: &str,
    ) -> anyhow::Result<Vec<(String, String)>>;
}

/// 转义为 JSON 字符串字面量（含引号，供 Prompt 组包用）。
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 去 markdown 代码围栏（LLM 常包 ```json ... ``` 返回）与首尾空白。
fn strip_fences(body: &str) -> String {
    let mut s = body.trim().to_owned();
    for fence in ["```json", "```JSON", "```"] {
        if let Some(rest) = s.strip_prefix(fence) {
            s = rest.to_owned();
            break;
        }
    }
    if let Some(rest) = s.strip_suffix("```") {
        s = rest.to_owned();
    }
    s.trim().to_owned()
}

/// 兜底扫描：提取所有 `"id": 数字 … "translation": "…"` 对（顺序即出现顺序）。
///
/// 手写扫描器（不引入 `regex`）：处理 `\"`、`\\`、`\n` 等常见转义与 `\uXXXX`。
fn scan_id_translations(s: &str) -> Vec<TextTranslation> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // 找 "id" 键
        let Some(key_pos) = find_key(s, i, "id") else { break };
        let mut j = key_pos;
        // 跳过冒号取数字
        if !skip_to_colon(bytes, &mut j) {
            i = key_pos + 1;
            continue;
        }
        let num_start = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        let Ok(id) = s[num_start..j].parse::<usize>() else {
            i = key_pos + 1;
            continue;
        };
        // 在该对象剩余范围内找 "translation" 字符串值
        // （若先遇到下一个 "id"，说明本条缺字段，跳过防错配）。
        let t_pos = find_key(s, j, "translation");
        let next_id = find_key(s, j, "id");
        let Some(t_pos) = t_pos else {
            i = key_pos + 1;
            continue;
        };
        if next_id.is_some_and(|nid| nid < t_pos) {
            i = key_pos + 1;
            continue;
        };
        let mut k = t_pos;
        if !skip_to_colon(bytes, &mut k) {
            i = key_pos + 1;
            continue;
        }
        // 跳过空白，期望双引号开字符串
        while k < bytes.len() && (bytes[k] as char).is_whitespace() {
            k += 1;
        }
        if k >= bytes.len() || bytes[k] != b'"' {
            i = key_pos + 1;
            continue;
        }
        let (value, next) = read_quoted(s, k);
        out.push(TextTranslation { id, translation: value });
        i = next;
    }
    out
}

/// 兜底扫描：提取所有 `"original": "…" … "translation": "…"` 对。
fn scan_original_translation(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let Some(o_pos) = find_key(s, i, "original") else { break };
        let mut j = o_pos;
        if !skip_to_colon(bytes, &mut j) {
            i = o_pos + 1;
            continue;
        }
        while j < bytes.len() && (bytes[j] as char).is_whitespace() {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b'"' {
            i = o_pos + 1;
            continue;
        }
        let (original, after_o) = read_quoted(s, j);
        // 原文之后找最近的 "translation"（若先遇到下一个 "original" 则本条缺字段，跳过）。
        let t_pos = find_key(s, after_o, "translation");
        let next_o = find_key(s, after_o, "original");
        let Some(t_pos) = t_pos else {
            i = o_pos + 1;
            continue;
        };
        if next_o.is_some_and(|n| n < t_pos) {
            i = o_pos + 1;
            continue;
        };
        let mut k = t_pos;
        if !skip_to_colon(bytes, &mut k) {
            i = o_pos + 1;
            continue;
        }
        while k < bytes.len() && (bytes[k] as char).is_whitespace() {
            k += 1;
        }
        if k >= bytes.len() || bytes[k] != b'"' {
            i = o_pos + 1;
            continue;
        }
        let (translation, next) = read_quoted(s, k);
        out.push((original, translation));
        i = next;
    }
    out
}

/// 从 `from`（字节下标）起找 `"key"` 键，返回键名结束引号之后的首个下标。
fn find_key(s: &str, from: usize, key: &str) -> Option<usize> {
    let pat = format!("\"{key}\"");
    let rel = s.get(from..)?.find(&pat)?;
    Some(from + rel + pat.len())
}

/// 跳过空白、冒号、空白；成功返回 true 且 `i` 停在值起始处。
fn skip_to_colon(bytes: &[u8], i: &mut usize) -> bool {
    while *i < bytes.len() && (bytes[*i] as char).is_whitespace() {
        *i += 1;
    }
    if *i >= bytes.len() || bytes[*i] != b':' {
        return false;
    }
    *i += 1;
    while *i < bytes.len() && (bytes[*i] as char).is_whitespace() {
        *i += 1;
    }
    true
}

/// 读取 `s[start]`（必须为 `"`）起的 JSON 字符串，返回（解码值，结束引号后下标）。
fn read_quoted(s: &str, start: usize) -> (String, usize) {
    let bytes = s.as_bytes();
    let mut out = String::new();
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return (out, i + 1),
            b'\\' if i + 1 < bytes.len() => {
                match bytes[i + 1] {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' if i + 5 < bytes.len() => {
                        if let Ok(cp) = u32::from_str_radix(&s[i + 2..i + 6], 16) {
                            out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                            i += 4;
                        } else {
                            out.push('u');
                        }
                    }
                    c => {
                        out.push('\\');
                        out.push(c as char);
                    }
                }
                i += 2;
            }
            _ => {
                // 多字节字符按 char 推进（避免把 UTF-8 切半）
                let ch = s[i..].chars().next().unwrap_or('\u{FFFD}');
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    (out, i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_prompt_keeps_ids_and_target() {
        let p = build_text_prompt(&[(1, "Hello"), (2, "世界")], "简体中文");
        assert!(p.contains("简体中文"));
        assert!(p.contains("\"id\": 1"));
        assert!(p.contains("不要合并或拆分"));
    }

    #[test]
    fn text_prompt_escapes_quotes() {
        let p = build_text_prompt(&[(1, "他说\"你好\"")], "English");
        // Prompt 里的 JSON 片段应合法（引号已转义）
        assert!(p.contains("\\\"你好\\\""));
    }

    #[test]
    fn multimodal_prompts_mention_schema() {
        assert!(build_multimodal_prompt("简体中文").contains("\"original\""));
        let batch = build_multimodal_batch_prompt("English", 3);
        assert!(batch.contains('3'));
        assert!(batch.contains("\"index\""));
    }

    #[test]
    fn parse_strict_text_array() {
        let items = parse_text_response(r#"[{"id": 1, "translation": "你好"}, {"id": 2, "translation": "世界"}]"#);
        assert_eq!(
            items,
            vec![
                TextTranslation { id: 1, translation: String::from("你好") },
                TextTranslation { id: 2, translation: String::from("世界") },
            ]
        );
    }

    #[test]
    fn parse_fenced_text_array() {
        let body = "```json\n[{\"id\": 1, \"translation\": \"你好\"}]\n```";
        let items = parse_text_response(body);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].translation, "你好");
    }

    #[test]
    fn parse_garbled_text_falls_back() {
        // 前言后语 + 单引号瑕疵：严格失败，兜底按出现顺序提取
        let body = "好的，这是翻译结果：{\"id\": 1, \"translation\": \"你好\"}，还有 {\"id\": 2, \"translation\": \"再见\"}，完毕。";
        let items = parse_text_response(body);
        assert_eq!(items.len(), 2);
        assert_eq!((items[0].id, items[1].id), (1, 2));
    }

    #[test]
    fn parse_empty_text_returns_empty() {
        assert!(parse_text_response("抱歉，我无法翻译。").is_empty());
        assert!(parse_text_response("").is_empty());
    }

    #[test]
    fn parse_multimodal_single_strict_and_fallback() {
        let ok = parse_multimodal_single(r#"{"original": "Hello", "translation": "你好"}"#);
        assert_eq!(
            ok,
            Some(ImageTranslation {
                original: String::from("Hello"),
                translation: String::from("你好"),
            })
        );
        // 非 JSON 包裹时兜底
        let fb = parse_multimodal_single("识别结果：{\"original\": \"Hi\", \"translation\": \"嗨\"}。");
        assert_eq!(fb.unwrap().translation, "嗨");
        assert!(parse_multimodal_single("图中没有文字").is_none());
    }

    #[test]
    fn parse_multimodal_batch_keeps_order() {
        let body = r#"[{"index": 0, "original": "A", "translation": "甲"}, {"index": 1, "original": "", "translation": ""}]"#;
        let items = parse_multimodal_batch(body);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].translation, "甲");
        assert_eq!(items[1].original, "");
    }

    #[test]
    fn fallback_handles_escapes() {
        let body = "结果 {\"id\": 7, \"translation\": \"他说\\\"你好\\\"\\n再见\"} 完毕";
        let items = parse_text_response(body);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].translation, "他说\"你好\"\n再见");
    }
}
