//! w1d1 — минимальный CLI: вопрос из аргументов → запрос в LLM → ответ в консоль.
//!
//! Провайдеры DeepSeek и OpenRouter совместимы с форматом OpenAI Chat Completions,
//! поэтому различаются только базовым URL, моделью и переменной с ключом.

use std::env;
use std::error::Error;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
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

/// Крутилка на время ожидания ответа: запрос блокирующий, поэтому анимация
/// живёт в отдельном потоке. Пишем в stderr, чтобы stdout остался чистым
/// ответом модели и его можно было перенаправить в файл.
fn start_spinner(stop: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let mut stderr = std::io::stderr();
        for frame in frames.iter().cycle() {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let _ = write!(stderr, "\r{frame} жду ответа...");
            let _ = stderr.flush();
            thread::sleep(Duration::from_millis(80));
        }
        let _ = write!(stderr, "\r\x1b[2K"); // стереть строку целиком
        let _ = stderr.flush();
    })
}

fn ask(provider: &Provider, api_key: &str, model: &str, question: &str) -> Result<String, Box<dyn Error>> {
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
    json["choices"][0]["message"]["content"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("не удалось разобрать ответ API: {body}").into())
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

    // Анимация нужна только живому терминалу, в пайпе или логе она мусор.
    let spinner = std::io::stderr().is_terminal().then(|| {
        let stop = Arc::new(AtomicBool::new(false));
        (Arc::clone(&stop), start_spinner(stop))
    });

    let answer = ask(&provider, &api_key, &model, &question);

    if let Some((stop, handle)) = spinner {
        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();
    }

    println!("{}", answer?);
    Ok(())
}
