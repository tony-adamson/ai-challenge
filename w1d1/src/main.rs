//! w1d1 — минимальный CLI: вопрос из аргументов → запрос в LLM → ответ в консоль.
//!
//! Провайдеры DeepSeek и OpenRouter совместимы с форматом OpenAI Chat Completions,
//! поэтому различаются только базовым URL, моделью и переменной с ключом.

use std::env;
use std::error::Error;
use std::time::Duration;

struct Provider {
    base_url: &'static str,
    default_model: &'static str,
    key_var: &'static str,
}

fn provider(name: &str) -> Result<Provider, String> {
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

fn main() -> Result<(), Box<dyn Error>> {
    // .env лежит в корне репозитория, dotenvy ищет его вверх по дереву каталогов.
    dotenvy::dotenv().ok();

    let question = env::args().skip(1).collect::<Vec<_>>().join(" ");
    if question.trim().is_empty() {
        eprintln!("Использование: cargo run -- \"твой вопрос\"");
        std::process::exit(2);
    }

    let provider_name = env::var("LLM_PROVIDER").unwrap_or_else(|_| "deepseek".to_string());
    let provider = provider(&provider_name)?;

    let api_key = env::var(provider.key_var).map_err(|_| {
        format!(
            "не задан {}: скопируй .env.example в .env и впиши ключ",
            provider.key_var
        )
    })?;

    let model = env::var("LLM_MODEL").unwrap_or_else(|_| provider.default_model.to_string());

    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?
        .post(format!("{}/chat/completions", provider.base_url))
        .bearer_auth(api_key)
        .json(&serde_json::json!({
            "model": model,
            "messages": [{ "role": "user", "content": question }],
        }))
        .send()?;

    let status = response.status();
    let body = response.text()?;
    if !status.is_success() {
        return Err(format!("API вернул {status}: {body}").into());
    }

    let json: serde_json::Value = serde_json::from_str(&body)?;
    let answer = json["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| format!("не удалось разобрать ответ API: {body}"))?;

    println!("{answer}");
    Ok(())
}
