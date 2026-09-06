//! 翻译双后端：Prompt 模板 + 宽松 JSON 解析（跨平台纯逻辑，见 AGENTS.md 3.8 节）。
//!
//! - 模式一（裁剪 → 多模态）：识别 + 翻译一体，输入裁剪小图；
//! - 模式二（OCR → 纯文本）：输入 `id + text` 数组，输出等长 `id + translation`；
//! - 解析宽松化：先去 markdown 代码围栏再 `serde_json` 严格解析，失败则用
//!   无依赖手写扫描器兜底提字段，再失败则丢弃该条（不影响其余区域）。
//! - 异步 `TranslationBackend` trait 与 HTTP 组包在 `llm/client.rs` 步骤落地，
//!   本模块只负责可单测的纯函数部分。

use serde::Deserialize;

use crate::config::TranslatePrompts;

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
///
/// 模板来自设置页（`TranslatePrompts`，空回退内置默认），占位符 `{target}` /
/// `{items}` 在此填充；未知占位符原样保留。
pub fn build_text_prompt(
    inputs: &[(usize, &str)],
    target_lang: &str,
    prompts: &TranslatePrompts,
) -> String {
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
    prompts
        .effective_text()
        .replace("{target}", target_lang)
        .replace("{items}", &items)
}

/// 模式一 Prompt：单张裁剪图，识别 + 翻译一体（模板占位符 `{target}`）。
pub fn build_multimodal_prompt(target_lang: &str, prompts: &TranslatePrompts) -> String {
    prompts.effective_multimodal_single().replace("{target}", target_lang)
}

/// 模式一 Prompt：多张裁剪图一次请求（要求按序返回数组 + 序号，防错位；
/// 模板占位符 `{target}` / `{count}`）。
pub fn build_multimodal_batch_prompt(
    target_lang: &str,
    image_count: usize,
    prompts: &TranslatePrompts,
) -> String {
    prompts
        .effective_multimodal_batch()
        .replace("{target}", target_lang)
        .replace("{count}", &image_count.to_string())
}



/// 严格解析成功的值也要过饰线清理：合法 JSON 里 `"\"A\" \"B\""` 这类
/// 行 framing 照样残留（单行超长 → 字号被压小，2026-09-06 实机根因）。
fn tidy_item(mut item: ImageTranslation) -> ImageTranslation {
    item.original = tidy_quotes(&item.original);
    item.translation = tidy_quotes(&item.translation);
    item
}

/// 解析模式二返回（数组，`id` 对齐； salvaged 条目按出现顺序返回）。
pub fn parse_text_response(body: &str) -> Vec<TextTranslation> {
    let clean = strip_fences(body);
    // 1. 严格解析（译文同样过饰线清理，见 `tidy_item`）。
    if let Ok(items) = serde_json::from_str::<Vec<TextTranslation>>(&clean) {
        return items
            .into_iter()
            .map(|mut t| {
                t.translation = tidy_quotes(&t.translation);
                t
            })
            .collect();
    }
    // 2. 兜底：扫描所有 {"id": N, "translation": "..."} 对。
    scan_id_translations(&clean)
}

/// 宽松化预处理：某些本地小模型会把整包转义输出（所有 `"` 写成 `\"`，
/// 严格解析必败）。仅当原文严格解析失败**且**含 `\"` 时，去转义后重试；
/// 合法包（含字符串内正常转义）走不到这里，不受影响。
/// 注意：去转义只用于严格重试，扫描一律用原文——去转义会把
/// `"\"A\""`（裸开引号 + 转义内容）变成 `""A""`，扫描读出空串
/// （2026-09-06 实机教训，见 `skip_escaped_open`）。
fn unescape_if_needed(clean: &str) -> Option<String> {
    if !clean.contains("\\\"") {
        return None;
    }
    Some(clean.replace("\\\"", "\""))
}

/// `from` 起跳过空白后若是 `,` / `}` / `]` / 结尾，说明当前位置是值的
/// 真边界（调用方在 `\"` 处探到这里时，那个 `\"` 就是闭引号）。
fn closes_value(bytes: &[u8], mut from: usize) -> bool {
    while from < bytes.len()
        && (bytes[from] == b' '
            || bytes[from] == b'\t'
            || bytes[from] == b'\n'
            || bytes[from] == b'\r')
    {
        from += 1;
    }
    from >= bytes.len() || bytes[from] == b',' || bytes[from] == b'}' || bytes[from] == b']'
}

/// 越过转义开引号：值以 `\"` 开头（整包被转义的写法）时跳过反斜杠，
/// 调用方便可按普通 `"` 处理。返回是否越过。
fn skip_escaped_open(bytes: &[u8], i: &mut usize) -> bool {
    if *i + 1 < bytes.len() && bytes[*i] == b'\\' && bytes[*i + 1] == b'"' {
        *i += 1;
        true
    } else {
        false
    }
}

/// 值开引号判定：ASCII `"`（1 字节）或中文开引号 `“`（U+201C，3 字节），
/// 返回引号字节长度（0 表示不是开引号）。本地模型常用中文引号包值
/// （`“教官？”“我不知道。”`），扫描器必须认（2026-09-06 实机：不认则整条丢弃报空）。
fn open_quote_len(bytes: &[u8], i: usize) -> usize {
    if i < bytes.len() && bytes[i] == b'"' {
        1
    } else if i + 3 <= bytes.len() && bytes[i] == 0xE2 && bytes[i + 1] == 0x80 && bytes[i + 2] == 0x9C
    {
        3
    } else {
        0
    }
}

/// 是否中文闭引号 `”`（U+201D，3 字节）起始位置。
fn is_close_curly(bytes: &[u8], i: usize) -> bool {
    i + 3 <= bytes.len() && bytes[i] == 0xE2 && bytes[i + 1] == 0x80 && bytes[i + 2] == 0x9D
}

/// 清理值两端的 ASCII 引号饰线：本地模型常用 `"第一行" "第二行"` 包行，
/// 解码后残留首尾 `"` 与行间 `" "`。处理：去首尾空白后，若首尾恰为一对
/// `"` 则剥掉一层；再把行间 `" + 空白 + "` 切成换行（对话体 `"hi"` 因
/// 紧贴单词不受影响）。干净值原样返回。
fn tidy_quotes(value: &str) -> String {
    let mut s = value.trim().to_string();
    // 首尾孤引号是 framing，各剥一层（独立判断：`"...` / `..."` 这种半边
    // 残留最常见，对话体用 “” 不受影响；行内引文紧贴单词，下面切不到它）。
    if s.starts_with('"') {
        s = s[1..].to_string();
    }
    if s.ends_with('"') {
        s = s[..s.len() - 1].to_string();
    }
    // 行间 `" + 空白 + "` 是模型的分行符，切成换行；行内引文
    // （如 `said "hi" loudly`，引号紧贴单词）不受影响。
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '"' {
            let mut j = i + 1;
            while j < chars.len() && (chars[j] == ' ' || chars[j] == '\t') {
                j += 1;
            }
            if j < chars.len() && chars[j] == '"' {
                if !out.trim_end().is_empty() {
                    out.push('\n');
                }
                i = j + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 解析模式一单图返回。
pub fn parse_multimodal_single(body: &str) -> Option<ImageTranslation> {
    let clean = strip_fences(body);
    if let Ok(item) = serde_json::from_str::<ImageTranslation>(&clean) {
        return Some(tidy_item(item));
    }
    // 整包被转义时去转义再严格试一次；严格仍失败则扫描原文——扫描器
    // 自己处理转义开引号与饰线（去转义后扫描反而会把 `"\"A\""` 读成空串）。
    if let Some(unescaped) = unescape_if_needed(&clean) {
        if let Ok(item) = serde_json::from_str::<ImageTranslation>(&unescaped) {
            return Some(tidy_item(item));
        }
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
        return items.into_iter().map(tidy_item).collect();
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
            .map(|i| {
                tidy_item(ImageTranslation {
                    original: i.original,
                    translation: i.translation,
                })
            })
            .collect();
    }
    // 扫描用原文（理由同单图路径：去转义会毒化 `"\"…"` 开头的值）
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
        // 跳过空白，期望引号开字符串（`\"` 转义开引号 / 中文 `“` 先越过识别）
        while k < bytes.len() && (bytes[k] as char).is_whitespace() {
            k += 1;
        }
        skip_escaped_open(bytes, &mut k);
        if open_quote_len(bytes, k) == 0 {
            i = key_pos + 1;
            continue;
        }
        let (value, next) = read_quoted_concat(s, k);
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
        skip_escaped_open(bytes, &mut j);
        if open_quote_len(bytes, j) == 0 {
            i = o_pos + 1;
            continue;
        }
        let (original, after_o) = read_quoted_concat(s, j);
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
        skip_escaped_open(bytes, &mut k);
        if open_quote_len(bytes, k) == 0 {
            i = o_pos + 1;
            continue;
        }
        let (translation, next) = read_quoted_concat(s, k);
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

/// 读取 `s[start]`（必须为 `"`）起的 JSON 字符串，遇到**同行**相邻串
/// （`"a" "b"`，本地小模型把多行各包一对引号的常见写法）自动换行拼接。
///
/// 只吃同行相邻：段间含换行即停；续串之后若是冒号说明那是个键
/// （如 `"translation": …`），停住不吃（2026-09-06 实机教训）。
/// 空段不占行，整体去首尾空白。返回（解码值，结束下标）。
fn read_quoted_concat(s: &str, start: usize) -> (String, usize) {
    let bytes = s.as_bytes();
    let (first, mut i) = read_quoted(s, start);
    let mut parts = vec![first];
    loop {
        // 同行空白（空格/制表，不含换行）
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        // 续串也可以是转义开引号（整包转义写法里的 `"a" \"b\"` 混排）
        // 或中文开引号（`“第二行”` 紧跟）
        skip_escaped_open(bytes, &mut i);
        if open_quote_len(bytes, i) == 0 {
            break;
        }
        let (more, next) = read_quoted(s, i);
        // 续串不能跨行：杂散引号（如上段尾多余的 `"`）会一路吃到下一行的
        // `"translation"` 键，把键吞进值里导致整条作废（2026-09-06 实机）。
        if more.contains('\n') {
            break;
        }
        // 续串不能是纯结构符：尾部杂散 `"` 读出的 `}`/`,` 说明已撞上
        // JSON 结构，直接停（2026-09-06 实机：译文尾多出 `\n}`）。
        if !more.trim().is_empty()
            && more.trim().chars().all(|c| matches!(c, '}' | ']' | ','))
        {
            break;
        }
        // 前瞻：续串之后是冒号 → 这是个键，停住不吃
        let mut k = next;
        while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') {
            k += 1;
        }
        if k < bytes.len() && bytes[k] == b':' {
            break;
        }
        parts.push(more);
        i = next;
    }
    let text = parts
        .iter()
        .map(String::as_str)
        .filter(|p| !p.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    // 转义解码残留的引号饰线（如 `"第一行" "第二行"`）在此清理，
    // 干净值原样返回（见 `tidy_quotes`）。
    (tidy_quotes(&text), i)
}

/// 读取 `s[start]`（必须为 `"` 或中文 `“`）起的 JSON 字符串，
/// 返回（解码值，结束引号后下标）。
///
/// 宽松点：`\"` 之后（隔空白）紧跟 `,` / `}` / `]` / 结尾时视为闭引号
/// （本地模型常用 `\"行尾\",` 收尾，严格已失败的残包里这就是真边界；
/// 合法包走严格解析，根本到不了这里，故不误伤）。
/// 中文引号配对：`“` 开则闭于 `”`（本地模型常用中文引号包值）；
/// `"` 开的字符串里的 `”` 只是普通字符（对话体不受影响）。
fn read_quoted(s: &str, start: usize) -> (String, usize) {
    let bytes = s.as_bytes();
    let curly = open_quote_len(bytes, start) == 3;
    let mut out = String::new();
    let mut i = start + open_quote_len(bytes, start).max(1);
    while i < bytes.len() {
        // 中文闭引号（仅 `“` 开启时认）
        if curly && is_close_curly(bytes, i) {
            return (out, i + 3);
        }
        match bytes[i] {
            b'"' => return (out, i + 1),
            b'\\' if i + 1 < bytes.len() => {
                // `\"` 后紧跟结构符即视为闭引号（见函数文档）
                if bytes[i + 1] == b'"' && closes_value(bytes, i + 2) {
                    return (out, i + 2);
                }
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

    fn prompts() -> TranslatePrompts {
        TranslatePrompts::default()
    }

    #[test]
    fn text_prompt_keeps_ids_and_target() {
        let p = build_text_prompt(&[(1, "Hello"), (2, "世界")], "简体中文", &prompts());
        assert!(p.contains("简体中文"));
        assert!(p.contains("\"id\": 1"));
        assert!(p.contains("不要合并或拆分"));
    }

    #[test]
    fn text_prompt_escapes_quotes() {
        let p = build_text_prompt(&[(1, "他说\"你好\"")], "English", &prompts());
        // Prompt 里的 JSON 片段应合法（引号已转义）
        assert!(p.contains("\\\"你好\\\""));
    }

    #[test]
    fn multimodal_prompts_mention_schema() {
        assert!(build_multimodal_prompt("简体中文", &prompts()).contains("\"original\""));
        let batch = build_multimodal_batch_prompt("English", 3, &prompts());
        assert!(batch.contains('3'));
        assert!(batch.contains("\"index\""));
    }

    #[test]
    fn escaped_whole_payload_still_parses() {
        // 2026-09-06 实机回包：本地模型把整包转义（所有引号写成 \"），
        // 且把两行各包一对引号并排。必须是完整原文+译文，不能空。
        let body = "```json\n{\n    \"original\": \\\"Drill Instructor, what's wrong?\\\" \\\"I don't know.\\\"\",\n    \"translation\": \\\"教官，出什么事了？\\\" \\\"我不知道。\\\"\"\n}\n```";
        let item = parse_multimodal_single(body).expect("应解析出结果");
        assert!(item.original.contains("Drill Instructor"), "原文丢了：{item:?}");
        assert!(item.original.contains("I don't know"), "第二行丢了：{item:?}");
        assert!(item.translation.contains("教官"), "译文丢了：{item:?}");
        assert!(item.translation.contains("我不知道"), "译文第二行丢了：{item:?}");
    }


        #[test]
    fn real_log_escaped_adjacent_parses_fully() {
        // 2026-09-06 实机回包原样：整包转义 + 裸开引号 + 两行各包一对引号并排。
        // 必须完整拿回两行原文/译文（无残留引号），之前版本这里报空。
        let body = "```json\n{\n    \"original\": \"\\\"Drill Instructor, what's wrong?\\\" \\\"I don't know.\\\"\",\n    \"translation\": \"\\\"教官，出什么事了？\\\" \\\"我不知道。\\\"\"\n}\n```";
        let item = parse_multimodal_single(body).expect("应解析出结果");
        assert_eq!(item.original, "Drill Instructor, what's wrong?\nI don't know.");
        assert_eq!(item.translation, "教官，出什么事了？\n我不知道。");
    }

    #[test]
    fn fully_escaped_payload_parses() {
        // 整包转义、无裸引号：`\"A\" \"B\"` → 开引号越过 + 拼接。
        let body = r#"{"original": \"Line one\" \"Line two\", "translation": \"甲\" \"乙\"}"#;
        let item = parse_multimodal_single(body).expect("应解析出结果");
        assert_eq!(item.original, "Line one\nLine two");
        assert_eq!(item.translation, "甲\n乙");
    }

    #[test]
    fn curly_quoted_values_parse() {
        // 2026-09-06 实机：本地模型用中文引号包值（`“甲”“乙”`），不能整条丢弃报空。
        // 中文引号运行时拼接（源码字面量写法被本工具链拒收，见实测）。
        let (lq, rq) = (char::from_u32(0x201C).unwrap(), char::from_u32(0x201D).unwrap());
        let body = format!("{{\"original\": {lq}L1{rq}{lq}L2{rq}, \"translation\": {lq}甲{rq}{lq}乙{rq}}}");
        let item = parse_multimodal_single(&body).expect("应解析出结果");
        assert_eq!(item.original, "L1\nL2");
        assert_eq!(item.translation, "甲\n乙");
    }

    #[test]
    fn curly_quotes_inside_proper_json_survive() {
        // 合法 JSON 里行内中文引号是内容：strict 成功 + tidy 不动它们。
        let (lq, rq) = (char::from_u32(0x201C).unwrap(), char::from_u32(0x201D).unwrap());
        let body =
            format!("{{\"original\": \"Hi\", \"translation\": \"他说{lq}你好{rq}今天\"}}");
        let item = parse_multimodal_single(&body).expect("应解析出结果");
        let expect = format!("他说{lq}你好{rq}今天");
        assert_eq!(item.translation, expect);
    }

    #[test]
    fn real_log_curly_values_parse_fully() {
        // 2026-09-06 10:42 实机回包形状：值用中文引号包裹 + 尾部多余 ASCII 引号
        // （此前整条丢弃报空）。断言两行完整拿回、无残留引号。
        let (lq, rq) = (char::from_u32(0x201C).unwrap(), char::from_u32(0x201D).unwrap());
        let body = format!(
            "{{\"original\": {lq}A?{rq}{lq}B.{rq}, \"translation\": {lq}甲？{rq}{lq}乙。{rq}\"}}"
        );
        let item = parse_multimodal_single(&body).expect("应解析出结果");
        assert_eq!(item.original, "A?\nB.");
        assert_eq!(item.translation, "甲？\n乙。");
    }

    #[test]
    fn dialogue_quotes_are_preserved() {
        // 行内引文紧贴单词，不会被当成分行符切掉。
        let body = r#"{"original": "Hi", "translation": "他说 \"你好\" 今天"}"#;
        let item = parse_multimodal_single(&body).expect("应解析出结果");
        assert_eq!(item.translation, "他说 \"你好\" 今天");
    }




    #[test]
    fn adjacent_strings_join_with_newline() {
        // 同行相邻串拼接；但换行后的键不能被吞掉
        let body = r#"{"original": "ab" "cd", "translation": "甲" "乙"}"#;
        let item = parse_multimodal_single(&body).expect("应解析出结果");
        assert_eq!(item.original, "ab\ncd");
        assert_eq!(item.translation, "甲\n乙");
    }

    #[test]
    fn custom_templates_fill_placeholders() {
        let custom = TranslatePrompts {
            text: String::from("译为{target}：{items}"),
            multimodal_single: String::from("看图译为{target}"),
            multimodal_batch: String::from("共{count}图译为{target}"),
        };
        let p = build_text_prompt(&[(7, "Hi")], "法语", &custom);
        assert_eq!(p, "译为法语：{\"id\": 7, \"text\": \"Hi\"}");
        assert_eq!(build_multimodal_prompt("法语", &custom), "看图译为法语");
        assert_eq!(
            build_multimodal_batch_prompt("法语", 2, &custom),
            "共2图译为法语"
        );
    }

    #[test]
    fn empty_templates_fall_back_to_default() {
        let empty = TranslatePrompts {
            text: String::new(),
            multimodal_single: String::new(),
            multimodal_batch: String::new(),
        };
        let p = build_text_prompt(&[(1, "Hi")], "简体中文", &empty);
        assert!(p.contains("不要合并或拆分"));
        assert!(build_multimodal_prompt("简体中文", &empty).contains("\"original\""));
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
