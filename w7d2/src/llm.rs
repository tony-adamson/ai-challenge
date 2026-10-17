//! Мозг Эми: Gemini Flash через OpenAI-совместимый эндпоинт, при его ошибке — DeepSeek.
use serde_json::{json, Value};
use std::time::Duration;

struct Provider {
    name: &'static str,
    url: &'static str,
    model: &'static str,
    key: String,
}

pub struct Llm {
    providers: Vec<Provider>,
    http: reqwest::Client,
}

/// Ответ модели и кто его дал; `fallback` — почему пришлось уйти с первого провайдера.
pub struct Reply {
    pub message: Value,
    pub provider: &'static str,
    pub index: usize,
    pub fallback: Option<String>,
}

impl Llm {
    pub fn from_env() -> Result<Llm, String> {
        let key = |var: &str| std::env::var(var).ok().filter(|k| !k.trim().is_empty());
        let mut providers = Vec::new();
        if let Some(key) = key("GEMINI_API_KEY") {
            providers.push(Provider {
                name: "gemini",
                url: "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions",
                model: "gemini-3.8-flash",
                key,
            });
        }
        if let Some(key) = key("DEEPSEEK_API_KEY") {
            providers.push(Provider { name: "deepseek", url: "https://api.deepseek.com/v1/chat/completions", model: "deepseek-v4-flash", key });
        }
        if providers.is_empty() {
            return Err("нет ключей LLM: задай GEMINI_API_KEY и/или DEEPSEEK_API_KEY в .env в корне репозитория".into());
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(90))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Llm { providers, http })
    }

    pub fn names(&self) -> String {
        self.providers.iter().map(|p| format!("{} ({})", p.name, p.model)).collect::<Vec<_>>().join(" → ")
    }

    /// Провайдеры по очереди, начиная с `from`: надёжность важнее скорости, поэтому любая ошибка — повод спросить следующего.
    pub async fn chat(&self, from: usize, messages: &[Value], tools: &[Value]) -> Result<Reply, String> {
        let mut errors = Vec::new();
        for (index, p) in self.providers.iter().enumerate().skip(from) {
            match self.call(p, messages, tools).await {
                Ok(message) => {
                    let fallback = (!errors.is_empty()).then(|| errors.join("; "));
                    return Ok(Reply { message, provider: p.name, index, fallback });
                }
                Err(e) => {
                    eprintln!("LLM {}: {e}", p.name);
                    errors.push(format!("{}: {e}", p.name));
                }
            }
        }
        Err(format!("ни один провайдер не ответил — {}", errors.join("; ")))
    }

    async fn call(&self, p: &Provider, messages: &[Value], tools: &[Value]) -> Result<Value, String> {
        // Подписи размышлений Gemini (extra_content) нужны только ему; после fallback чужой провайдер может отвергнуть неизвестное поле.
        let messages: Vec<Value> = if p.name == "gemini" { messages.to_vec() } else { messages.iter().map(strip_extra).collect() };
        // 4000: у Gemini 3 размышления входят в лимит, на 2000 JSON ревью обрывался посередине (w7d2).
        let mut body = json!({ "model": p.model, "messages": messages, "max_tokens": 4000, "temperature": 0.2 });
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        // Иначе DeepSeek тратит лимит на reasoning_content и присылает пустой content (урок w6d3).
        if p.name == "deepseek" {
            body["thinking"] = json!({ "type": "disabled" });
        }
        let response = self.http.post(p.url).bearer_auth(&p.key).json(&body).send().await.map_err(|e| format!("нет связи: {e}"))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| format!("ответ оборвался: {e}"))?;
        if !status.is_success() {
            return Err(format!("HTTP {status}: {}", text.chars().take(300).collect::<String>()));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("ответ не JSON ({e})"))?;
        let message = v["choices"][0]["message"].clone();
        let empty = message["content"].as_str().is_none_or(|s| s.trim().is_empty());
        if !message.is_object() || (empty && message["tool_calls"].as_array().is_none_or(Vec::is_empty)) {
            return Err(format!("пустой ответ: {}", text.chars().take(300).collect::<String>()));
        }
        Ok(message)
    }
}

fn strip_extra(message: &Value) -> Value {
    let mut m = message.clone();
    if let Some(obj) = m.as_object_mut() {
        obj.remove("extra_content");
    }
    if let Some(calls) = m["tool_calls"].as_array_mut() {
        for call in calls.iter_mut().filter_map(Value::as_object_mut) {
            call.remove("extra_content");
        }
    }
    m
}
