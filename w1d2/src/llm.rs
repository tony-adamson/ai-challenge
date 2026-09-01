//! Запрос к LLM. Провайдеры DeepSeek и OpenRouter совместимы с форматом
//! OpenAI Chat Completions, поэтому различаются базовым URL, моделью и
//! переменной с ключом.

use std::error::Error;
use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::{json, Value};

pub struct Provider {
    pub base_url: &'static str,
    pub default_model: &'static str,
    pub key_var: &'static str,
}

pub fn provider(name: &str) -> Result<Provider, String> {
    match name {
        "deepseek" => Ok(Provider {
            base_url: "https://api.deepseek.com/v1",
            default_model: "deepseek-v4-flash",
            key_var: "DEEPSEEK_API_KEY",
        }),
        "openrouter" => Ok(Provider {
            base_url: "https://openrouter.ai/api/v1",
            default_model: "deepseek/deepseek-v4-flash",
            key_var: "OPENROUTER_API_KEY",
        }),
        other => Err(format!(
            "неизвестный LLM_PROVIDER: {other} (ожидается deepseek или openrouter)"
        )),
    }
}

/// Параметры одного вызова. Здесь живут ровно те рычаги, которые задание
/// требует показать на стороне API: лимит длины, стоп-последовательность
/// и режим JSON.
pub struct Request<'a> {
    pub model: &'a str,
    pub messages: Vec<Value>,
    pub max_tokens: Option<u32>,
    pub stop: Option<&'a str>,
    pub json_mode: bool,
}

/// Ответ модели вместе с тем, как он завершился.
///
/// `finish_reason` важен не меньше текста: `stop` — модель закончила сама или
/// упёрлась в стоп-последовательность, `length` — упёрлась в max_tokens.
pub struct Answer {
    pub content: String,
    pub finish_reason: String,
    /// Токены рассуждения: у reasoning-моделей они тратятся из того же
    /// бюджета max_tokens, поэтому маленький лимит может съесть весь ответ.
    pub reasoning_tokens: u64,
}

/// Таймаут на один вызов. Ответ не стримится, а reasoning-модель успевает
/// долго думать до первого символа, поэтому 60 секунд мало для базового
/// запроса без max_tokens.
///
/// Важно: reqwest::blocking применяет таймаут дважды — к send() и к чтению
/// тела, — так что худший случай ожидания вдвое больше указанного здесь.
const TIMEOUT: Duration = Duration::from_secs(90);

/// Один клиент на всё время работы: внутри блокирующего клиента живёт
/// собственный рантайм, а пересоздание на каждый запрос means новое
/// TLS-рукопожатие вместо keep-alive.
pub fn build_client() -> Result<Client, Box<dyn Error>> {
    Ok(Client::builder().timeout(TIMEOUT).build()?)
}

pub fn ask(
    client: &Client,
    provider: &Provider,
    api_key: &str,
    request: &Request,
) -> Result<Answer, Box<dyn Error>> {
    let mut body = json!({
        "model": request.model,
        "messages": request.messages,
    });

    if let Some(limit) = request.max_tokens {
        body["max_tokens"] = json!(limit);
    }
    if let Some(marker) = request.stop {
        body["stop"] = json!([marker]);
    }
    if request.json_mode {
        body["response_format"] = json!({ "type": "json_object" });
    }

    let response = client
        .post(format!("{}/chat/completions", provider.base_url))
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .map_err(describe)?;

    let status = response.status();
    let text = response.text().map_err(describe)?;
    if !status.is_success() {
        // Тело ошибки приходит от чужого сервиса и может быть каким угодно:
        // в терминал пускаем только начало.
        return Err(format!("API вернул {status}: {}", truncate(&text, 500)).into());
    }

    let parsed: Value = serde_json::from_str(&text)?;
    let choice = &parsed["choices"][0];
    if choice.is_null() {
        return Err(format!("не удалось разобрать ответ API: {}", truncate(&text, 500)).into());
    }

    Ok(Answer {
        content: choice["message"]["content"].as_str().unwrap_or("").to_string(),
        finish_reason: choice["finish_reason"].as_str().unwrap_or("?").to_string(),
        reasoning_tokens: parsed["usage"]["completion_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .unwrap_or(0),
    })
}

/// Разные сетевые беды выглядят одинаково, если печатать только Display
/// от reqwest::Error. Здесь причина называется словами.
fn describe(err: reqwest::Error) -> Box<dyn Error> {
    let cause = if err.is_timeout() {
        format!("превышено ожидание ({} с на этап)", TIMEOUT.as_secs())
    } else if err.is_connect() {
        "не удалось соединиться с API (сеть, DNS или TLS)".to_string()
    } else if err.is_decode() {
        "ответ API не читается".to_string()
    } else {
        "сбой запроса".to_string()
    };

    let mut detail = err.to_string();
    let mut source = err.source();
    while let Some(inner) = source {
        detail.push_str(&format!(" ← {inner}"));
        source = inner.source();
    }
    format!("{cause}: {detail}").into()
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

pub fn assistant(content: &str) -> Value {
    json!({ "role": "assistant", "content": content })
}
