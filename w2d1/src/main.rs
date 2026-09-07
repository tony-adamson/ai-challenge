//! w2d1 — веб-чат с одним агентом на Cerebras. Здесь только HTTP: маршруты,
//! общее состояние и переклад событий агента в SSE. Настройки, история и
//! формат API живут в `agent.rs`.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};

mod agent;

use agent::{Agent, Event, Settings, DEFAULT_SYSTEM_PROMPT, MODELS};

const ADDR: &str = "127.0.0.1:8790";

#[derive(Deserialize)]
struct AskRequest {
    text: String,
}

#[tokio::main]
async fn main() {
    if let Err(err) = start().await {
        eprintln!("Ошибка: {err}");
        std::process::exit(1);
    }
}

async fn start() -> Result<(), String> {
    println!("AI Advent · w2d1 — агент на Cerebras");
    match dotenvy::dotenv() {
        Ok(path) => println!("  ✓ .env прочитан: {}", path.display()),
        Err(_) => println!("  · .env не найден, беру переменные окружения"),
    }
    let api_key = std::env::var("CEREBRAS_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or("не задан CEREBRAS_API_KEY: скопируй .env.example в .env и впиши ключ")?;

    let agent = Arc::new(Agent::new(api_key)?);
    let settings = agent.settings();
    println!("  ✓ модель: {} · рассуждение: {}", settings.model, settings.reasoning_effort);

    let router = Router::new()
        .route("/", get(|| async { Html(include_str!("../static/index.html")) }))
        .route("/static/colors_and_type.css", get(css))
        .route("/static/fonts/Manrope-VariableFont_wght.ttf", get(font))
        .route("/static/cerebras.svg", get(logo))
        .route("/api/state", get(state))
        .route("/api/settings", post(settings_update))
        .route("/api/reset", post(reset))
        .route("/api/ask", post(ask))
        .with_state(agent);

    let listener = tokio::net::TcpListener::bind(ADDR)
        .await
        .map_err(|e| format!("не удалось занять порт {ADDR}: {e}"))?;
    let url = format!("http://{ADDR}");
    println!("  ✓ чат: {url}\nCtrl-C — выход\n");
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(&url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(&url).spawn();

    axum::serve(listener, router).await.map_err(|e| format!("сервер остановился: {e}"))
}

// Статика вшита в бинарник: своего сервера файлов ради трёх файлов не нужно.
async fn css() -> ([(header::HeaderName, &'static str); 1], &'static str) {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], include_str!("../static/colors_and_type.css"))
}

async fn font() -> ([(header::HeaderName, &'static str); 1], &'static [u8]) {
    ([(header::CONTENT_TYPE, "font/ttf")], include_bytes!("../static/fonts/Manrope-VariableFont_wght.ttf"))
}

async fn logo() -> ([(header::HeaderName, &'static str); 1], &'static str) {
    ([(header::CONTENT_TYPE, "image/svg+xml")], include_str!("../static/cerebras.svg"))
}

/// Всё, что нужно странице при загрузке: панель и переписка восстанавливаются
/// из состояния агента, а не из памяти браузера.
async fn state(State(agent): State<Arc<Agent>>) -> Json<Value> {
    let models: Vec<Value> = MODELS
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "label": m.label,
                "tok_s_hint": m.tok_s_hint,
                "price_in": m.price_in,
                "price_out": m.price_out,
            })
        })
        .collect();
    Json(json!({
        "settings": agent.settings(),
        "history": agent.history(),
        "models": models,
        "default_system_prompt": DEFAULT_SYSTEM_PROMPT,
    }))
}

async fn settings_update(
    State(agent): State<Arc<Agent>>,
    Json(settings): Json<Settings>,
) -> (StatusCode, String) {
    match agent.update_settings(settings) {
        Ok(()) => (StatusCode::OK, "сохранено".to_string()),
        Err(error) => (StatusCode::BAD_REQUEST, error),
    }
}

async fn reset(State(agent): State<Arc<Agent>>) -> (StatusCode, String) {
    agent.reset();
    (StatusCode::OK, "контекст очищен".to_string())
}

/// Один вопрос — один поток событий. Дельты уходят JSON-строкой: перевод
/// строки внутри текста иначе разорвал бы событие SSE на два.
async fn ask(
    State(agent): State<Arc<Agent>>,
    Json(request): Json<AskRequest>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(async move { agent.ask(request.text, tx).await });

    let stream = ReceiverStream::new(rx).map(|event| {
        let (name, data) = match event {
            Event::Reasoning(delta) => ("reasoning", json!(delta)),
            Event::Content(delta) => ("content", json!(delta)),
            Event::Done(metrics) => ("done", json!(metrics)),
            Event::Error(error) => ("error", json!(error)),
        };
        Ok(SseEvent::default().event(name).data(data.to_string()))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
