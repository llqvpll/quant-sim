//! 大模型客户端：直连 OpenAI 兼容的 /chat/completions 接口，不依赖任何本地编排服务。
//!
//! 换服务商只改 base_url 与 model，支持 DeepSeek、Qwen、Kimi 与本地 Ollama。
//! DeepSeek 用户零配置即可使用：Key 缺失时回落到 `DEEPSEEK_API_KEY`，
//! 地址与模型也有对应回落。
//!
//! 安全约定：API Key 只从环境变量读取，绝不写入 state.json，日志一律脱敏。

use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::http::HttpClient;
use crate::signal::{build_prompt, parse_signal, AiSignal, AnalyzeRequest, GroundOptions};
use crate::types::AiSource;
use crate::util::log;

const ENV_BASE_URL: &str = "QUANT_SIM_LLM_BASE_URL";
const ENV_API_KEY: &str = "QUANT_SIM_LLM_API_KEY";
const ENV_MODEL: &str = "QUANT_SIM_LLM_MODEL";
/// DeepSeek 官方变量名，让存量用户不改环境变量就能跑。
const ENV_DEEPSEEK_BASE_URL: &str = "DEEPSEEK_BASE_URL";
const ENV_DEEPSEEK_API_KEY: &str = "DEEPSEEK_API_KEY";
const ENV_DEEPSEEK_MODEL: &str = "DEEPSEEK_MODEL";
const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
const DEFAULT_MODEL: &str = "deepseek-chat";
/// 低温保证信号稳定，避免同一只股票每次给出相反结论。
const DEFAULT_TEMPERATURE: f64 = 0.3;

#[derive(Clone)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout_secs: u64,
    /// 期望的重试次数。真正的重试在 HTTP 层统一执行（最多 3 次尝试）。
    pub max_retries: u32,
    pub temperature: f64,
}

impl std::fmt::Debug for LlmConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &mask_key(&self.api_key))
            .field("model", &self.model)
            .field("timeout_secs", &self.timeout_secs)
            .field("max_retries", &self.max_retries)
            .field("temperature", &self.temperature)
            .finish()
    }
}

impl LlmConfig {
    pub fn from_env(cfg: &Config) -> AppResult<LlmConfig> {
        Self::resolve(&|name| std::env::var(name).ok(), cfg)
    }

    /// 解析逻辑与读取来源解耦，方便单测覆盖优先级而不污染进程环境变量。
    pub fn resolve(get: &dyn Fn(&str) -> Option<String>, cfg: &Config) -> AppResult<LlmConfig> {
        let api_key =
            first_env(get, &[ENV_API_KEY, ENV_DEEPSEEK_API_KEY]).ok_or_else(missing_key_error)?;
        let base_url = first_env(get, &[ENV_BASE_URL, ENV_DEEPSEEK_BASE_URL])
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        let model = first_env(get, &[ENV_MODEL, ENV_DEEPSEEK_MODEL])
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        Ok(LlmConfig {
            base_url,
            api_key,
            model,
            timeout_secs: cfg.ai_timeout_seconds as u64,
            max_retries: cfg.ai_max_retries,
            temperature: DEFAULT_TEMPERATURE,
        })
    }

    pub fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }
}

fn first_env(get: &dyn Fn(&str) -> Option<String>, names: &[&str]) -> Option<String> {
    for name in names {
        if let Some(value) = get(name) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn missing_key_error() -> AppError {
    AppError::Llm(format!(
        "未找到大模型 API Key。请设置环境变量 {ENV_API_KEY}（DeepSeek 用户也可直接设置 {ENV_DEEPSEEK_API_KEY}）。\
示例：PowerShell 执行 $env:{ENV_API_KEY}=\"sk-xxxx\"；bash 执行 export {ENV_API_KEY}=sk-xxxx"
    ))
}

/// 日志里只保留可辨识的前缀，任何情况下不打印完整 Key。
pub fn mask_key(key: &str) -> String {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return "(未设置)".to_string();
    }
    if trimmed.chars().count() <= 6 {
        return "***".to_string();
    }
    let head: String = trimmed.chars().take(3).collect();
    format!("{head}***")
}

#[derive(Clone, Debug, Default)]
pub struct LlmReply {
    pub text: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

pub struct LlmClient {
    http: HttpClient,
    cfg: LlmConfig,
}

impl LlmClient {
    pub fn new(http: HttpClient, cfg: LlmConfig) -> Self {
        log(&format!(
            "[AI分析] 大模型 {} @ {} (key={})",
            cfg.model,
            cfg.base_url,
            mask_key(&cfg.api_key)
        ));
        LlmClient { http, cfg }
    }

    /// 请求体固定带 `response_format=json_object`：我们只要结构化信号，
    /// 不要模型的客套话。
    pub fn build_body(&self, prompt: &str) -> serde_json::Value {
        serde_json::json!({
            "model": self.cfg.model,
            "messages": [{"role": "user", "content": prompt}],
            "response_format": {"type": "json_object"},
            "temperature": self.cfg.temperature,
            "stream": false,
        })
    }

    pub fn chat_json(&self, prompt: &str) -> AppResult<LlmReply> {
        let body = self.build_body(prompt);
        let headers = vec![
            (
                "Authorization".to_string(),
                format!("Bearer {}", self.cfg.api_key),
            ),
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "application/json".to_string()),
        ];
        let (_, value) = self.http.post_json(&self.cfg.endpoint(), &headers, &body)?;

        let text = value
            .pointer("/choices/0/message/content")
            .and_then(|node| node.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if text.is_empty() {
            return Err(AppError::Llm("大模型返回内容为空".to_string()));
        }
        Ok(LlmReply {
            text,
            prompt_tokens: token_count(&value, "prompt_tokens"),
            completion_tokens: token_count(&value, "completion_tokens"),
        })
    }
}

fn token_count(value: &serde_json::Value, key: &str) -> u64 {
    value
        .pointer(&format!("/usage/{key}"))
        .and_then(|node| node.as_u64())
        .unwrap_or(0)
}

impl AiSource for LlmClient {
    fn analyze(&self, req: &AnalyzeRequest<'_>) -> AiSignal {
        let prompt = build_prompt(req);
        match self.chat_json(&prompt) {
            Ok(reply) => {
                let opts = GroundOptions {
                    verify_price: req.verify_price,
                    expected_name: req.expected_name.map(|name| name.to_string()),
                    ..Default::default()
                };
                let signal = parse_signal(&reply.text, req.symbol, &opts);
                log(&format!(
                    "[AI分析] {} -> {} 置信 {:.2} 趋势 {} (tokens in/out {}/{})",
                    req.symbol,
                    signal.signal.as_str(),
                    signal.confidence,
                    signal.trend.as_deref().unwrap_or("-"),
                    reply.prompt_tokens,
                    reply.completion_tokens
                ));
                signal
            }
            Err(err) => {
                // 调用失败不 panic：返回 error 信号，本轮跳过该标的即可。
                log(&format!("[AI分析] {} 调用失败: {err}", req.symbol));
                AiSignal::error(req.symbol, &format!("AI 调用失败: {err}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    /// 环境变量读取器：测试里用 HashMap 模拟，避免并发改进程环境。
    type EnvGetter = Box<dyn Fn(&str) -> Option<String>>;

    fn cfg_with(get: &[(&str, &str)]) -> (EnvGetter, Config) {
        let map: HashMap<String, String> = get
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        let getter = move |name: &str| map.get(name).cloned();
        (Box::new(getter), Config::default())
    }

    fn client() -> LlmClient {
        let http = HttpClient::new(Duration::from_secs(5), Duration::from_secs(5)).unwrap();
        let cfg = LlmConfig {
            base_url: "https://api.deepseek.com".to_string(),
            api_key: "sk-test".to_string(),
            model: "deepseek-chat".to_string(),
            timeout_secs: 180,
            max_retries: 3,
            temperature: DEFAULT_TEMPERATURE,
        };
        LlmClient::new(http, cfg)
    }

    #[test]
    fn quant_sim_env_wins_over_deepseek_env() {
        let (get, cfg) = cfg_with(&[
            ("QUANT_SIM_LLM_API_KEY", "sk-quant"),
            ("DEEPSEEK_API_KEY", "sk-deepseek"),
            ("QUANT_SIM_LLM_MODEL", "my-model"),
            ("DEEPSEEK_MODEL", "deepseek-chat"),
            ("QUANT_SIM_LLM_BASE_URL", "https://example.com/v1/"),
        ]);
        let resolved = LlmConfig::resolve(get.as_ref(), &cfg).unwrap();
        assert_eq!(resolved.api_key, "sk-quant");
        assert_eq!(resolved.model, "my-model");
        assert_eq!(resolved.base_url, "https://example.com/v1");
    }

    #[test]
    fn deepseek_env_is_fallback_with_defaults() {
        let (get, cfg) = cfg_with(&[("DEEPSEEK_API_KEY", "sk-deepseek")]);
        let resolved = LlmConfig::resolve(get.as_ref(), &cfg).unwrap();
        assert_eq!(resolved.api_key, "sk-deepseek");
        assert_eq!(resolved.base_url, "https://api.deepseek.com");
        assert_eq!(resolved.model, "deepseek-chat");
        assert_eq!(
            resolved.endpoint(),
            "https://api.deepseek.com/chat/completions"
        );
    }

    #[test]
    fn missing_key_returns_readable_error_without_panic() {
        let (get, cfg) = cfg_with(&[]);
        let err = LlmConfig::resolve(get.as_ref(), &cfg).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("QUANT_SIM_LLM_API_KEY"));
        assert!(message.contains("DEEPSEEK_API_KEY"));
        assert!(matches!(err, AppError::Llm(_)));
    }

    #[test]
    fn blank_env_values_are_ignored() {
        let (get, cfg) = cfg_with(&[
            ("QUANT_SIM_LLM_API_KEY", "   "),
            ("DEEPSEEK_API_KEY", "sk-deepseek"),
        ]);
        let resolved = LlmConfig::resolve(get.as_ref(), &cfg).unwrap();
        assert_eq!(resolved.api_key, "sk-deepseek");
    }

    /// D1：base_url 独立回落（只设 DEEPSEEK_BASE_URL 时生效）。
    #[test]
    fn base_url_falls_back_to_deepseek_env() {
        let (get, cfg) = cfg_with(&[
            ("QUANT_SIM_LLM_API_KEY", "sk-quant"),
            ("DEEPSEEK_BASE_URL", "https://api.moonshot.cn/v1"),
        ]);
        let resolved = LlmConfig::resolve(get.as_ref(), &cfg).unwrap();
        assert_eq!(resolved.base_url, "https://api.moonshot.cn/v1");
        assert_eq!(
            resolved.endpoint(),
            "https://api.moonshot.cn/v1/chat/completions"
        );
        // 未设 model 时回落默认
        assert_eq!(resolved.model, "deepseek-chat");
    }

    /// D1：model 独立回落（只设 DEEPSEEK_MODEL 时生效）。
    #[test]
    fn model_falls_back_to_deepseek_env() {
        let (get, cfg) = cfg_with(&[
            ("QUANT_SIM_LLM_API_KEY", "sk-quant"),
            ("DEEPSEEK_MODEL", "deepseek-reasoner"),
        ]);
        let resolved = LlmConfig::resolve(get.as_ref(), &cfg).unwrap();
        assert_eq!(resolved.model, "deepseek-reasoner");
        // 未设 base 时回落默认地址
        assert_eq!(resolved.base_url, "https://api.deepseek.com");
    }

    #[test]
    fn request_body_is_json_object_mode() {
        let body = client().build_body("分析 600519");
        assert_eq!(body["model"], "deepseek-chat");
        assert_eq!(body["response_format"]["type"], "json_object");
        assert_eq!(body["stream"], serde_json::Value::Bool(false));
        assert_eq!(body["temperature"], 0.3);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "分析 600519");
        // 请求体里不能出现 Key
        assert!(!body.to_string().contains("sk-test"));
    }

    #[test]
    fn api_key_is_masked() {
        assert_eq!(mask_key("sk-1234567890abcdef"), "sk-***");
        assert_eq!(mask_key("short"), "***");
        assert_eq!(mask_key(""), "(未设置)");
        let (get, cfg) = cfg_with(&[("QUANT_SIM_LLM_API_KEY", "sk-secret-value")]);
        let resolved = LlmConfig::resolve(get.as_ref(), &cfg).unwrap();
        let debug = format!("{resolved:?}");
        assert!(debug.contains("sk-***"));
        assert!(!debug.contains("sk-secret-value"));
    }

    #[test]
    fn analyze_without_network_returns_error_signal() {
        // 指向不可解析的地址：必须返回 error 信号而不是 panic。
        let http = HttpClient::new(Duration::from_millis(300), Duration::from_millis(300)).unwrap();
        let cfg = LlmConfig {
            base_url: "http://127.0.0.1:1".to_string(),
            api_key: "sk-test".to_string(),
            model: "deepseek-chat".to_string(),
            timeout_secs: 1,
            max_retries: 3,
            temperature: DEFAULT_TEMPERATURE,
        };
        let client = LlmClient::new(http, cfg);
        let req = AnalyzeRequest::new("600519", "现在适合买入吗？");
        let signal = client.analyze(&req);
        assert_eq!(signal.signal, crate::signal::SignalAction::Error);
        assert!(signal.retryable);
        assert!(signal.reason.contains("调用失败"));
    }
}
