//! LLM HTTP 客户端（OpenAI 兼容格式，见 AGENTS.md 3.8 节）。
//!
//! - [`LlmClient`]: `reqwest` 薄封装（chat 文本 / chat 多模态），60s 超时，失败重试 1 次；
//! - [`TextBackend`]: [`TranslationBackend`] 的纯文本实现（模式二）；
//! - [`MultimodalBackend`]: [`TranslationBackend`] 的多模态实现（模式一）。
//!
//! Prompt 模板与返回解析复用 [`crate::translate::backend`] 的纯函数（已单测），
//! 本模块只做 HTTP 组包/发送与结果对齐。异步任务由调用方 `tokio::spawn` 丢到后台，
//! 结果经 `EventLoopProxy` 回传 UI（AGENTS.md 3.10 节）。

use base64::Engine as _;

use crate::config::LlmEndpoint;
use crate::translate::backend::{
    self, ImageTranslation, TextTranslation, TranslationBackend,
};

/// 单次请求超时（秒）；传输失败自动重试 1 次（共 2 次尝试）。
const REQUEST_TIMEOUT_SECS: u64 = 60;
/// 请求重试次数（不含首次）。
const REQUEST_RETRIES: usize = 1;
/// 裁剪图长边上限（像素，超限等比缩小，省图片 token）。
const MAX_IMAGE_SIDE: u32 = 1024;
/// 裁剪图 JPEG 质量（OCR/识别够用即可，不追求无损）。
const JPEG_QUALITY: u8 = 80;
/// 大模型调用参数走配置（`TranslateConfig::params_json`，设置页"大模型参数"
/// 卡片直接编辑 JSON；未知字段服务端一般忽略，非法回退内置默认）。
/// OpenAI 兼容 chat 客户端（`{url}/v1/chat/completions`，Key 为空则不带认证头）。
pub struct LlmClient {
    http: reqwest::Client,
    endpoint: LlmEndpoint,
    /// 调用参数 JSON（对象字符串，全后端共用）。
    params_json: String,
    /// 思考模式总开关（`false` = 从请求体剔掉思考四键）。
    disable_thinking: bool,
}

impl LlmClient {
    /// 由配置构造（`api_url` 为空时后续请求直接报错，不在这里拦）。
    pub fn new(
        endpoint: LlmEndpoint,
        params_json: String,
        disable_thinking: bool,
    ) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()?;
        Ok(Self { http, endpoint, params_json, disable_thinking })
    }

    /// 纯文本 chat（system + user），返回 assistant 文本。
    pub async fn chat_text(&self, system: &str, user: &str) -> anyhow::Result<String> {
        let body = self.build_text_body(system, user);
        self.post(body).await
    }

    /// 纯文本请求体组包（纯函数，单测断言参数合并用）。
    fn build_text_body(&self, system: &str, user: &str) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": self.endpoint.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        });
        merge_params_body(
            &mut body,
            self.disable_thinking,
            &self.params_json,
        );
        body
    }

    /// 多模态 chat（prompt + JPEG 裁剪图），返回 assistant 文本。
    pub async fn chat_vision(
        &self,
        prompt: &str,
        crops: &[image::DynamicImage],
    ) -> anyhow::Result<String> {
        let mut content = vec![serde_json::json!({"type": "text", "text": prompt})];
        for crop in crops {
            let jpeg = encode_crop(crop)?;
            content.push(serde_json::json!({
                "type": "image_url",
                "image_url": {"url": data_url(&jpeg)},
            }));
        }
        let mut body = serde_json::json!({
            "model": self.endpoint.model,
            "messages": [
                {"role": "user", "content": content},
            ],
        });
        merge_params_body(
            &mut body,
            self.disable_thinking,
            &self.params_json,
        );
        self.post(body).await
    }

    /// 发送 chat 请求（传输失败重试，业务错误直接返回）。
    async fn post(&self, body: serde_json::Value) -> anyhow::Result<String> {
        if self.endpoint.api_url.trim().is_empty() {
            anyhow::bail!("LLM 地址未配置，请先在设置页填写");
        }
        if self.endpoint.model.trim().is_empty() {
            anyhow::bail!("LLM 模型未配置，请先在设置页填写");
        }
        let mut last_err = anyhow::anyhow!("未知请求错误");
        for _ in 0..=REQUEST_RETRIES {
            match self.post_once(&body).await {
                Ok(text) => return Ok(text),
                Err(e) => {
                    tracing::warn!("LLM 请求失败（将重试/降级）: {e:#}");
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    /// 单次发送（含返回解析）。
    async fn post_once(&self, body: &serde_json::Value) -> anyhow::Result<String> {
        let mut req = self.http.post(self.endpoint.api_url.as_str()).json(body);
        if !self.endpoint.api_key.trim().is_empty() {
            req = req.bearer_auth(self.endpoint.api_key.trim());
        }
        let resp = req.send().await?;
        let status = resp.status();
        let json: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            let msg = json
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("未知服务端错误");
            anyhow::bail!("LLM 返回 {status}: {msg}");
        }
        extract_content(&json)
    }
}

/// 把用户配置的参数 JSON 合并进请求体（2026-09-12 设置页"大模型参数"卡片）。
///
/// 合法 JSON 对象逐键合并（温度/上限/上下文/思考四件套/其他全由用户自配）；
/// 非法（非 JSON / 非对象 / 空对象）回退内置默认并记日志；总开关关闭时合并后
/// 再剔掉思考四键（`THINKING_PARAM_KEYS`），用户自加的其他键不受影响。
/// 各家关思考参数不统一，默认 JSON 把四件套都带上，服务端一般忽略不认识的
/// 字段。llama.cpp 老版本顶层参数无效，请用服务端启动参数关思考。
/// 若某云端严格校验未知字段而 400，把总开关关掉并精简 JSON 即可。
fn merge_params_body(
    body: &mut serde_json::Value,
    disable_thinking: bool,
    params_json: &str,
) {
    // 自定义非法一律回退内置默认
    let fallback: serde_json::Value =
        serde_json::from_str(crate::config::DEFAULT_LLM_PARAMS_JSON)
            .unwrap_or(serde_json::Value::Object(Default::default()));
    let parsed: serde_json::Value = serde_json::from_str(params_json).unwrap_or_else(|e| {
        tracing::warn!("大模型参数 JSON 非法（{e}），回退内置默认");
        fallback.clone()
    });
    let mut custom = parsed
        .as_object()
        .cloned()
        .unwrap_or_else(Default::default);
    if custom.is_empty() {
        if let Some(def) = fallback.as_object() {
            custom = def.clone();
        }
    }
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    for (k, v) in &custom {
        obj.insert(k.clone(), v.clone());
    }
    // 总开关关闭：只剔思考四键，其余用户参数原样保留
    if !disable_thinking {
        for k in crate::config::THINKING_PARAM_KEYS {
            obj.remove(k);
        }
    }
}

/// 剥掉思考过程块（`<think>…</think>`，含未闭合的半截）。
///
/// 兜底：关思考参数是"尽力"语义，某些后端仍会吐思考块；思考文本混入译文会
/// 污染下游 JSON 解析，故在出口统一清理。无标签时原样返回，零成本。
/// 大小写不敏感（`<THINK>` 同理），嵌套不考虑（模型只吐一层）。
fn strip_think_blocks(text: &str) -> String {
    let lower = text.to_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut rest = 0;
    // 字节下标与字符边界：只在 `<` 处切分，`<` 恒为单字节 ASCII，安全
    while let Some(rel) = lower[rest..].find("<think>") {
        let start = rest + rel;
        out.push_str(&text[rest..start]);
        let after_open = start + "<think>".len();
        if let Some(end_rel) = lower[after_open..].find("</think>") {
            rest = after_open + end_rel + "</think>".len();
        } else {
            // 未闭合：后面全是思考，直接丢弃并结束
            rest = text.len();
            break;
        }
    }
    out.push_str(&text[rest..]);
    // 残留的孤立闭标签（如只有 </think>）一并清理
    let cleaned = out.replace("</think>", "").replace("</THINK>", "");
    cleaned.trim().to_string()
}

/// 从 chat completions 返回中提取 assistant 文本。
///
/// 兼容 `{"choices": [{"message": {"content": "..."}}]}` 标准形状；
/// `content` 为数组（部分多模态服务端）时拼接其中 `text` 段。
/// 出口统一过 `strip_think_blocks`（关不掉思考的后端兜底）。
pub fn extract_content(body: &serde_json::Value) -> anyhow::Result<String> {
    let content = body
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .ok_or_else(|| {
            let snippet: String = body.to_string().chars().take(200).collect();
            anyhow::anyhow!("LLM 返回缺少 choices/message/content: {snippet}")
        })?;
    if let Some(text) = content.as_str() {
        return Ok(strip_think_blocks(text));
    }
    // 数组形状：拼接各 text 段
    let mut out = String::new();
    if let Some(parts) = content.as_array() {
        for p in parts {
            if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                out.push_str(t);
            }
        }
    }
    if out.is_empty() {
        anyhow::bail!("LLM 返回 content 为空或形状未知")
    } else {
        Ok(strip_think_blocks(&out))
    }
}

/// 裁剪图编码：长边超限等比缩小后转 JPEG 字节（省 token，识别够用）。
pub fn encode_crop(image: &image::DynamicImage) -> anyhow::Result<Vec<u8>> {
    let rgb = image.to_rgb8();
    let (w, h) = (rgb.width(), rgb.height());
    let rgb = if w.max(h) > MAX_IMAGE_SIDE {
        image::imageops::resize(
            &rgb,
            w.min(MAX_IMAGE_SIDE),
            h.min(MAX_IMAGE_SIDE),
            image::imageops::FilterType::Triangle,
        )
    } else {
        rgb
    };
    let mut buf = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY);
    enc.encode_image(&rgb)?;
    Ok(buf)
}

/// 日志截断（按字符数，避免长回包刷屏；中文按字符计）。
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect::<String>() + "…"
}

/// JPEG 字节包成 `data:image/jpeg;base64,...` URL（OpenAI 图片输入格式）。
pub fn data_url(jpeg: &[u8]) -> String {
    format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(jpeg)
    )
}

/// 纯文本翻译后端（模式二）。
pub struct TextBackend {
    client: LlmClient,
    prompts: crate::config::TranslatePrompts,
}

impl TextBackend {
    /// 由文本 LLM 配置 + 提示词模板 + 调用参数构造。
    pub fn new(
        endpoint: LlmEndpoint,
        prompts: crate::config::TranslatePrompts,
        params_json: String,
        disable_thinking: bool,
    ) -> anyhow::Result<Self> {
        Ok(Self { client: LlmClient::new(endpoint, params_json, disable_thinking)?, prompts })
    }
}

#[async_trait::async_trait]
impl TranslationBackend for TextBackend {
    async fn translate_text(
        &self,
        texts: &[String],
        target_lang: &str,
    ) -> anyhow::Result<Vec<String>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let inputs: Vec<(usize, &str)> =
            texts.iter().enumerate().map(|(i, t)| (i + 1, t.as_str())).collect();
        let prompt = backend::build_text_prompt(&inputs, target_lang, &self.prompts);
        tracing::debug!("纯文本翻译请求：{} 条，目标 {}", texts.len(), target_lang);
        let body = self
            .client
            .chat_text("你是一个只输出 JSON 的专业翻译助手。", &prompt)
            .await?;
        tracing::debug!("纯文本翻译回包（{} 字节）：{}", body.len(), truncate(&body, 800));
        let items = backend::parse_text_response(&body);
        let out = align_by_id(texts, &items);
        let empty = out.iter().filter(|t| t.trim().is_empty()).count();
        if !out.is_empty() && empty == out.len() {
            tracing::warn!("纯文本翻译全空：输入 {texts:?}，回包 {body:?}");
        }
        Ok(out)
    }

    async fn recognize_and_translate_image(
        &self,
        _crops: &[image::DynamicImage],
        _target_lang: &str,
    ) -> anyhow::Result<Vec<(String, String)>> {
        anyhow::bail!("纯文本后端不支持图片输入（管线不会如此调度，属防御分支）")
    }
}

/// id 对齐回填：缺失条目用原文兜底（不丢内容、不错位）。
fn align_by_id(texts: &[String], items: &[TextTranslation]) -> Vec<String> {
    use std::collections::HashMap;
    let map: HashMap<usize, &str> =
        items.iter().map(|t| (t.id, t.translation.as_str())).collect();
    texts
        .iter()
        .enumerate()
        .map(|(i, original)| {
            map.get(&(i + 1)).map(|s| (*s).to_owned()).unwrap_or_else(|| original.clone())
        })
        .collect()
}

/// 多模态识别 + 翻译后端（模式一）。
pub struct MultimodalBackend {
    client: LlmClient,
    prompts: crate::config::TranslatePrompts,
}

impl MultimodalBackend {
    /// 由多模态 LLM 配置 + 提示词模板 + 调用参数构造。
    pub fn new(
        endpoint: LlmEndpoint,
        prompts: crate::config::TranslatePrompts,
        params_json: String,
        disable_thinking: bool,
    ) -> anyhow::Result<Self> {
        Ok(Self { client: LlmClient::new(endpoint, params_json, disable_thinking)?, prompts })
    }
}

#[async_trait::async_trait]
impl TranslationBackend for MultimodalBackend {
    async fn translate_text(
        &self,
        _texts: &[String],
        _target_lang: &str,
    ) -> anyhow::Result<Vec<String>> {
        anyhow::bail!("多模态后端不支持纯文本输入（管线不会如此调度，属防御分支）")
    }

    async fn recognize_and_translate_image(
        &self,
        crops: &[image::DynamicImage],
        target_lang: &str,
    ) -> anyhow::Result<Vec<(String, String)>> {
        if crops.is_empty() {
            return Ok(vec![]);
        }
        if crops.len() == 1 {
            let prompt = backend::build_multimodal_prompt(target_lang, &self.prompts);
            tracing::debug!("多模态单图翻译请求，目标 {}", target_lang);
            let body = self.client.chat_vision(&prompt, crops).await?;
            tracing::debug!("多模态单图回包（{} 字节）：{}", body.len(), truncate(&body, 800));
            let single = backend::parse_multimodal_single(&body).unwrap_or(ImageTranslation {
                original: String::new(),
                translation: String::new(),
            });
            if single.translation.trim().is_empty() {
                tracing::warn!("多模态单图译文为空，回包 {body:?}");
            }
            return Ok(vec![(single.original, single.translation)]);
        }
        let prompt =
            backend::build_multimodal_batch_prompt(target_lang, crops.len(), &self.prompts);
        tracing::debug!("多模态批量翻译请求：{} 图，目标 {}", crops.len(), target_lang);
        let body = self.client.chat_vision(&prompt, crops).await?;
        tracing::debug!("多模态批量回包（{} 字节）：{}", body.len(), truncate(&body, 800));
        let items = backend::parse_multimodal_batch(&body);
        // 按下标对齐；解析丢条时尾部补空对（管线跳过不覆盖）。
        let mut out = Vec::with_capacity(crops.len());
        for i in 0..crops.len() {
            match items.get(i) {
                Some(it) => out.push((it.original.clone(), it.translation.clone())),
                None => out.push((String::new(), String::new())),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_standard_shape() {
        let body = serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "你好"}}],
        });
        assert_eq!(extract_content(&body).unwrap(), "你好");
    }

    #[test]
    fn extract_array_content_shape() {
        let body = serde_json::json!({
            "choices": [{"message": {"content": [
                {"type": "text", "text": "甲"},
                {"type": "text", "text": "乙"},
            ]}}],
        });
        assert_eq!(extract_content(&body).unwrap(), "甲乙");
    }

    #[test]
    fn extract_missing_shape_errors() {
        let body = serde_json::json!({"error": {"message": "bad"}});
        assert!(extract_content(&body).is_err());
    }

    #[test]
    fn encode_crop_downscales_and_data_url() {
        let big = image::DynamicImage::new_rgb8(2048, 512);
        let bytes = encode_crop(&big).unwrap();
        // JPEG 魔数
        assert_eq!(&bytes[0..2], &[0xFF, 0xD8]);
        let url = data_url(&bytes);
        assert!(url.starts_with("data:image/jpeg;base64,"));
        // 长边应被压到 1024 以内（解码验尺寸）
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert!(decoded.width().max(decoded.height()) <= MAX_IMAGE_SIDE);
    }

    #[test]
    fn align_by_id_falls_back_to_original() {
        let texts = vec![String::from("A"), String::from("B"), String::from("C")];
        let items = vec![TextTranslation { id: 2, translation: String::from("乙") }];
        assert_eq!(align_by_id(&texts, &items), vec!["A", "乙", "C"]);
    }

    #[test]
    fn params_merge_all_keys_and_thinking_switch() {
        // 总开关开：全部键合并（含温度/上限/上下文/思考四件套）
        let mut body = serde_json::json!({"model": "m", "messages": []});
        merge_params_body(
            &mut body,
            true,
            crate::config::DEFAULT_LLM_PARAMS_JSON,
        );
        assert!((body["temperature"].as_f64().unwrap_or(-1.0) - 0.2).abs() < 1e-9);
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["num_ctx"], 8192);
        assert_eq!(body["reasoning_effort"], "none");
        assert_eq!(body["model"], "m");
        // 总开关关：只剔思考四键，其余保留
        let mut body = serde_json::json!({"model": "m"});
        merge_params_body(
            &mut body,
            false,
            crate::config::DEFAULT_LLM_PARAMS_JSON,
        );
        assert!((body["temperature"].as_f64().unwrap_or(-1.0) - 0.2).abs() < 1e-9);
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("enable_thinking").is_none());
        assert!(body.get("chat_template_kwargs").is_none());
        assert!(body.get("think").is_none());
    }

    #[test]
    fn params_merge_custom_and_fallback() {
        // 自定义 JSON 按原样合并
        let mut body = serde_json::json!({"model": "m"});
        merge_params_body(&mut body, true, r#"{"temperature": 0.7, "my_opt": 1}"#);
        assert!((body["temperature"].as_f64().unwrap_or(-1.0) - 0.7).abs() < 1e-9);
        assert_eq!(body["my_opt"], 1);
        // 非法 JSON 回退内置默认
        let mut body = serde_json::json!({"model": "m"});
        merge_params_body(&mut body, true, "{broken");
        assert_eq!(body["reasoning_effort"], "none");
        // 非对象/空对象回退内置默认
        let mut body = serde_json::json!({"model": "m"});
        merge_params_body(&mut body, true, "[1,2]");
        assert_eq!(body["think"], false);
        let mut body = serde_json::json!({"model": "m"});
        merge_params_body(&mut body, true, "{}");
        assert_eq!(body["enable_thinking"], false);
    }

    #[test]
    fn client_body_uses_configured_params() {
        let client = LlmClient::new(
            LlmEndpoint::default(),
            String::from(r#"{"temperature": 0.7, "max_tokens": 512}"#),
            false,
        )
        .unwrap();
        let body = client.build_text_body("s", "u");
        assert!((body["temperature"].as_f64().unwrap_or(-1.0) - 0.7).abs() < 1e-9);
        assert_eq!(body["max_tokens"], 512);
        // 总开关关 → 无思考键
        assert!(body.get("think").is_none());
    }

    #[test]
    fn strip_think_blocks_pair_and_unclosed() {
        assert_eq!(
            strip_think_blocks("<think>嗯……</think>你好"),
            "你好"
        );
        assert_eq!(
            strip_think_blocks("前<think>没想完"),
            "前"
        );
        // 无标签原样（仅去首尾空白）
        assert_eq!(strip_think_blocks("  甲乙  "), "甲乙");
        // 大小写不敏感
        assert_eq!(
            strip_think_blocks("<THINK>x</THINK>好"),
            "好"
        );
    }

    #[test]
    fn extract_strips_think_from_content() {
        let body = serde_json::json!({
            "choices": [{"message": {"content": "<think>翻译中</think>{\"a\":1}"}}],
        });
        assert_eq!(extract_content(&body).unwrap(), "{\"a\":1}");
    }

    #[test]
    fn client_rejects_empty_config() {        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let client = LlmClient::new(
                LlmEndpoint::default(),
                String::from(crate::config::DEFAULT_LLM_PARAMS_JSON),
                true,
            )
            .unwrap();
            // 默认地址非空但模型为空 → 模型未配置错误（不发网）
            let err = client.chat_text("s", "u").await.unwrap_err();
            assert!(err.to_string().contains("模型未配置"), "{err:#}");
        });
    }
}
