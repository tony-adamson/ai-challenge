//! Потоковый запрос к LLM. DeepSeek и OpenRouter совместимы с форматом
//! OpenAI Chat Completions, поэтому различаются только базовым URL,
//! названием модели и ключом. Ответ читается по SSE-чанкам, чтобы токены
//! появлялись на экране по мере генерации.

use std::time::Duration;

use reqwest::Client;
use serde_json::{json, Value};

#[derive(Clone)]
pub struct Model {
    /// Короткий идентификатор для UI и событий: `primary` / `second`.
    pub id: String,
    pub label: String,
    pub base_url: String,
    pub name: String,
    pub api_key: String,
}

fn env_nonempty(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.trim().is_empty())
}

/// Основная модель берётся из `LLM_PROVIDER`/`LLM_MODEL`, как в прошлые дни.
/// Вторая, нерассуждающая, подключается через OpenRouter, если есть ключ:
/// на ней видно, помогает ли «решай пошагово» модели без встроенного
/// рассуждения.
pub fn models_from_env() -> Result<Vec<Model>, String> {
    let provider = std::env::var("LLM_PROVIDER").unwrap_or_else(|_| "deepseek".to_string());
    let (base_url, default_model, key_var) = match provider.as_str() {
        "deepseek" => ("https://api.deepseek.com/v1", "deepseek-v4-flash", "DEEPSEEK_API_KEY"),
        "openrouter" => ("https://openrouter.ai/api/v1", "deepseek/deepseek-v4-flash", "OPENROUTER_API_KEY"),
        other => {
            return Err(format!(
                "неизвестный LLM_PROVIDER: {other} (ожидается deepseek или openrouter)"
            ))
        }
    };
    let api_key = env_nonempty(key_var)
        .ok_or_else(|| format!("не задан {key_var}: скопируй .env.example в .env и впиши ключ"))?;
    let name = env_nonempty("LLM_MODEL").unwrap_or_else(|| default_model.to_string());

    let mut models = vec![Model {
        id: "primary".to_string(),
        label: format!("{provider} · {name}"),
        base_url: base_url.to_string(),
        name,
        api_key,
    }];

    if let Some(api_key) = env_nonempty("OPENROUTER_API_KEY") {
        let name = env_nonempty("SECOND_MODEL").unwrap_or_else(|| "openai/gpt-4.1-nano".to_string());
        let duplicate = provider == "openrouter" && models[0].name == name;
        if !duplicate {
            models.push(Model {
                id: "second".to_string(),
                label: format!("openrouter · {name}"),
                base_url: "https://openrouter.ai/api/v1".to_string(),
                name,
                api_key,
            });
        }
    }
    Ok(models)
}

pub struct Answer {
    pub content: String,
    pub completion_tokens: u64,
    /// Токены встроенного рассуждения. Если их нет — модель отвечает «в лоб».
    pub reasoning_tokens: u64,
}

/// Таймаут на весь потоковый ответ: консилиум из трёх экспертов плюс
/// модератор на reasoning-модели может думать долго.
const TIMEOUT: Duration = Duration::from_secs(300);

pub fn build_client() -> Result<Client, String> {
    Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| format!("не удалось создать HTTP-клиент: {e}"))
}

/// Один потоковый вызов. `on_token` получает каждый кусок текста сразу,
/// как только он пришёл; второй аргумент — `true` для токенов встроенного
/// рассуждения (DeepSeek шлёт их в `reasoning_content`, OpenRouter — в
/// `reasoning`), они не входят в `content`.
pub async fn stream(
    client: &Client,
    model: &Model,
    messages: Vec<Value>,
    json_mode: bool,
    mut on_token: impl FnMut(&str, bool),
) -> Result<Answer, String> {
    let mut body = json!({
        "model": model.name,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if json_mode {
        body["response_format"] = json!({ "type": "json_object" });
    }
    if model.base_url.contains("openrouter") {
        // OpenRouter отдаёт usage в последнем чанке только по явной просьбе.
        body["usage"] = json!({ "include": true });
    }

    let mut response = client
        .post(format!("{}/chat/completions", model.base_url))
        .bearer_auth(&model.api_key)
        .json(&body)
        .send()
        .await
        .map_err(describe)?;

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        // Тело ошибки приходит от чужого сервиса: показываем только начало.
        return Err(format!("API вернул {status}: {}", truncate(&text, 500)));
    }

    let mut answer = Answer { content: String::new(), completion_tokens: 0, reasoning_tokens: 0 };
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(describe)? {
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" {
                continue;
            }
            let Ok(event) = serde_json::from_str::<Value>(data) else { continue };
            if let Some(err) = event.get("error") {
                return Err(format!("API вернул ошибку в потоке: {}", truncate(&err.to_string(), 500)));
            }
            let delta = &event["choices"][0]["delta"];
            if let Some(text) = delta["content"].as_str() {
                if !text.is_empty() {
                    answer.content.push_str(text);
                    on_token(text, false);
                }
            }
            for field in ["reasoning_content", "reasoning"] {
                if let Some(text) = delta[field].as_str() {
                    if !text.is_empty() {
                        on_token(text, true);
                    }
                }
            }
            if let Some(usage) = event["usage"].as_object() {
                answer.completion_tokens = usage["completion_tokens"].as_u64().unwrap_or(0);
                answer.reasoning_tokens = usage["completion_tokens_details"]["reasoning_tokens"]
                    .as_u64()
                    .unwrap_or(0);
            }
        }
    }

    if answer.content.trim().is_empty() {
        return Err("модель вернула пустой ответ".to_string());
    }
    Ok(answer)
}

/// Разные сетевые беды выглядят одинаково, если печатать только Display
/// от reqwest::Error. Здесь причина называется словами.
fn describe(err: reqwest::Error) -> String {
    let cause = if err.is_timeout() {
        format!("превышено ожидание ({} с)", TIMEOUT.as_secs())
    } else if err.is_connect() {
        "не удалось соединиться с API (сеть, DNS или TLS)".to_string()
    } else if err.is_decode() {
        "ответ API не читается".to_string()
    } else {
        "сбой запроса".to_string()
    };
    let mut detail = err.to_string();
    let mut source = std::error::Error::source(&err);
    while let Some(inner) = source {
        detail.push_str(&format!(" ← {inner}"));
        source = inner.source();
    }
    format!("{cause}: {detail}")
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}… (обрезано)")
}

pub fn user(content: &str) -> Value {
    json!({ "role": "user", "content": content })
}

pub fn system(content: &str) -> Value {
    json!({ "role": "system", "content": content })
}
