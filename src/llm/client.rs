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
/// 纯文本翻译温度（低随机，保证术语一致）。
const TEXT_TEMPERATURE: f32 = 0.2;
/// 返回上限（token，防长文截断；本地服务不识别该字段时一般忽略）。
const MAX_TOKENS: u32 = 4096;

/// OpenAI 兼容 chat 客户端（`{url}/v1/chat/completions`，Key 为空则不带认证头）。
pub struct LlmClient {
    http: reqwest::Client,
    endpoint: LlmEndpoint,
}

impl LlmClient {
    /// 由配置构造（`api_url` 为空时后续请求直接报错，不在这里拦）。
    pub fn new(endpoint: LlmEndpoint) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()?;
        Ok(Self { http, endpoint })
    }

    /// 纯文本 chat（system + user），返回 assistant 文本。
    pub async fn chat_text(&self, system: &str, user: &str) -> anyhow::Result<String> {
        let body = serde_json::json!({
            "model": self.endpoint.model,
            "temperature": TEXT_TEMPERATURE,
            "max_tokens": MAX_TOKENS,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        });
        self.post(body).await
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
        let body = serde_json::json!({
            "model": self.endpoint.model,
            "temperature": TEXT_TEMPERATURE,
            "max_tokens": MAX_TOKENS,
            "messages": [
                {"role": "user", "content": content},
            ],
        });
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

/// 从 chat completions 返回中提取 assistant 文本。
///
/// 兼容 `{"choices": [{"message": {"content": "..."}}]}` 标准形状；
/// `content` 为数组（部分多模态服务端）时拼接其中 `text` 段。
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
        return Ok(text.to_owned());
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
        Ok(out)
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
}

impl TextBackend {
    /// 由文本 LLM 配置构造。
    pub fn new(endpoint: LlmEndpoint) -> anyhow::Result<Self> {
        Ok(Self { client: LlmClient::new(endpoint)? })
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
        let prompt = backend::build_text_prompt(&inputs, target_lang);
        let body = self
            .client
            .chat_text("你是一个只输出 JSON 的专业翻译助手。", &prompt)
            .await?;
        Ok(align_by_id(texts, &backend::parse_text_response(&body)))
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
}

impl MultimodalBackend {
    /// 由多模态 LLM 配置构造。
    pub fn new(endpoint: LlmEndpoint) -> anyhow::Result<Self> {
        Ok(Self { client: LlmClient::new(endpoint)? })
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
            let prompt = backend::build_multimodal_prompt(target_lang);
            let body = self.client.chat_vision(&prompt, crops).await?;
            let single = backend::parse_multimodal_single(&body).unwrap_or(ImageTranslation {
                original: String::new(),
                translation: String::new(),
            });
            return Ok(vec![(single.original, single.translation)]);
        }
        let prompt = backend::build_multimodal_batch_prompt(target_lang, crops.len());
        let body = self.client.chat_vision(&prompt, crops).await?;
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
    fn client_rejects_empty_config() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let client = LlmClient::new(LlmEndpoint::default()).unwrap();
            // 默认地址非空但模型为空 → 模型未配置错误（不发网）
            let err = client.chat_text("s", "u").await.unwrap_err();
            assert!(err.to_string().contains("模型未配置"), "{err:#}");
        });
    }
}
