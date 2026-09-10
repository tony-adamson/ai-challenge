//! w2d4 — веб-чат, который сжимает историю. Здесь только HTTP: маршруты,
//! общее состояние и переклад событий агента в SSE. История лежит в файлах
//! (`store.rs`), формат API провайдеров, сжатие и вся арифметика токенов — в
//! `agent.rs`.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

mod agent;
mod store;

use agent::{is_context_overflow, Agent, Event, Provider, Settings, PERSONAS};
use store::{Chat, Store, DATA_DIR};

const ADDR: &str = "127.0.0.1:8793";

struct App {
    agent: Agent,
    store: Store,
}

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
    println!("AI Advent · w2d4 — агент, который сжимает историю");
    match dotenvy::dotenv() {
        Ok(path) => println!("  ✓ .env прочитан: {}", path.display()),
        Err(_) => println!("  · .env не найден, беру переменные окружения"),
    }

    let agent = Agent::new()?;
    for provider in Provider::ALL {
        match agent.is_available(provider) {
            true => println!("  ✓ провайдер: {}", provider.label()),
            false => println!("  · провайдер {} без ключа ({})", provider.label(), provider.key_env()),
        }
    }

    let store = Store::open(DATA_DIR)?;
    println!("  ✓ чатов на диске: {} (папка {DATA_DIR}/)", store.list().len());

    let app = Arc::new(App { agent, store });
    let router = Router::new()
        .route("/", get(|| async { Html(include_str!("../static/index.html")) }))
        .route("/static/colors_and_type.css", get(css))
        .route("/static/fonts/Manrope-VariableFont_wght.ttf", get(font))
        .route("/static/cerebras.svg", get(cerebras_logo))
        .route("/static/deepseek.svg", get(deepseek_logo))
        .route("/static/openrouter.svg", get(openrouter_logo))
        .route("/api/state", get(state))
        .route("/api/chats", post(create_chat))
        .route("/api/chats/{id}", get(open_chat))
        .route("/api/chats/{id}", delete(remove_chat))
        .route("/api/chats/{id}/settings", post(update_settings))
        .route("/api/chats/{id}/ask", post(ask))
        .with_state(app);

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

// Статика вшита в бинарник: своего сервера файлов ради четырёх файлов не нужно.
async fn css() -> ([(header::HeaderName, &'static str); 1], &'static str) {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], include_str!("../static/colors_and_type.css"))
}

async fn font() -> ([(header::HeaderName, &'static str); 1], &'static [u8]) {
    ([(header::CONTENT_TYPE, "font/ttf")], include_bytes!("../static/fonts/Manrope-VariableFont_wght.ttf"))
}

async fn cerebras_logo() -> ([(header::HeaderName, &'static str); 1], &'static str) {
    ([(header::CONTENT_TYPE, "image/svg+xml")], include_str!("../static/cerebras.svg"))
}

async fn deepseek_logo() -> ([(header::HeaderName, &'static str); 1], &'static str) {
    ([(header::CONTENT_TYPE, "image/svg+xml")], include_str!("../static/deepseek.svg"))
}

async fn openrouter_logo() -> ([(header::HeaderName, &'static str); 1], &'static str) {
    ([(header::CONTENT_TYPE, "image/svg+xml")], include_str!("../static/openrouter.svg"))
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "чат не найден").into_response()
}

/// Всё, что нужно странице при загрузке: список чатов, открытый чат,
/// справочник провайдеров с их моделями и уровнями рассуждения и список
/// личностей с шаблонами промптов.
async fn state(State(app): State<Arc<App>>) -> Json<Value> {
    let chats: Vec<Value> = app
        .store
        .list()
        .iter()
        .map(|c| {
            // Контекст и цена чата считаются на лету по сохранённым метрикам:
            // дублировать их отдельными полями в файле незачем, а список
            // всё равно читает чаты целиком.
            let metrics = c.messages.iter().filter_map(|m| m.metrics.as_ref());
            let last_prompt_tokens = metrics.clone().last().map_or(0, |m| m.prompt_tokens);
            let total_cost_usd: f64 = metrics.filter_map(|m| m.cost_usd).sum();
            json!({
                "id": c.id,
                "title": c.title,
                "updated_at": c.updated_at,
                "provider": c.settings.provider,
                "model": c.settings.model,
                "last_prompt_tokens": last_prompt_tokens,
                "total_cost_usd": total_cost_usd,
            })
        })
        .collect();

    let providers: Vec<Value> = Provider::ALL
        .into_iter()
        .map(|p| {
            let available = app.agent.is_available(p);
            json!({
                "id": p.id(),
                "label": p.label(),
                "available": available,
                "reason": (!available).then(|| format!("нет {} в .env", p.key_env())),
                "cost_from_api": p.cost_from_api(),
                "models": p.models().iter().map(|m| json!({
                    "id": m.id,
                    "label": m.label,
                    "tok_s_hint": m.tok_s_hint,
                    "price_in": m.price_in,
                    "price_out": m.price_out,
                    "context_window": m.context_window,
                    "max_tokens": m.max_tokens,
                })).collect::<Vec<_>>(),
                "reasoning_levels": p.reasoning_levels().iter()
                    .map(|(value, label)| json!({ "value": value, "label": label }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();

    Json(json!({
        "active_chat": app.store.active(),
        "chats": chats,
        "providers": providers,
        "personas": PERSONAS.iter()
            .map(|p| json!({ "id": p.id, "name": p.name, "prompt": p.prompt }))
            .collect::<Vec<_>>(),
    }))
}

/// Новый чат наследует настройки открытого: менять провайдера и промпт
/// заново на каждый чат — лишняя работа.
async fn create_chat(State(app): State<Arc<App>>) -> Response {
    let settings = app
        .store
        .active()
        .and_then(|id| app.store.load(&id))
        .map(|chat| chat.settings)
        .unwrap_or_else(|| app.agent.default_settings());

    let chat = Chat::new(settings);
    if let Err(error) = app.store.save(&chat) {
        return (StatusCode::INTERNAL_SERVER_ERROR, error).into_response();
    }
    if let Err(error) = app.store.set_active(&chat.id) {
        eprintln!("{error}");
    }
    Json(chat).into_response()
}

/// Открытие чата заодно двигает указатель: после F5 страница вернётся туда,
/// где её оставили.
async fn open_chat(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let Some(chat) = app.store.load(&id) else { return not_found() };
    if let Err(error) = app.store.set_active(&chat.id) {
        eprintln!("{error}");
    }
    Json(chat).into_response()
}

async fn remove_chat(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    if app.store.load(&id).is_none() {
        return not_found();
    }
    match app.store.delete(&id) {
        Ok(()) => Json(json!({ "active_chat": app.store.active() })).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}

async fn update_settings(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(settings): Json<Settings>,
) -> Response {
    if let Err(error) = settings.validate() {
        return (StatusCode::BAD_REQUEST, error).into_response();
    }
    let Some(mut chat) = app.store.load(&id) else { return not_found() };
    chat.settings = settings;
    match app.store.save(&chat) {
        Ok(()) => (StatusCode::OK, "сохранено").into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}

/// Один вопрос — один поток событий. Дельты уходят JSON-строкой: перевод
/// строки внутри текста иначе разорвал бы событие SSE на два. Чат
/// сохраняется здесь же, до отправки `done`: страница показывает успех
/// только тогда, когда ответ действительно лежит на диске.
async fn ask(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(request): Json<AskRequest>,
) -> Response {
    let Some(mut chat) = app.store.load(&id) else { return not_found() };

    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(async move {
        // Отклонённый запрос агент дописывает в историю записью `error`, и её
        // тоже надо сохранить. Отказы до самого запроса (пустой текст, занятый
        // чат) историю не трогают — такой чат перезаписывать незачем.
        let before = chat.messages.len();
        let event = match app.agent.ask(&mut chat, &request.text, &tx).await {
            Ok(metrics) => match app.store.save(&chat) {
                Ok(()) => Event::Done(metrics),
                Err(error) => Event::Error(format!("ответ получен, но чат не сохранён: {error}")),
            },
            Err(error) => {
                if chat.messages.len() != before {
                    if let Err(e) = app.store.save(&chat) {
                        eprintln!("отклонённый запрос не сохранён: {e}");
                    }
                }
                Event::Error(error)
            }
        };
        let answered = matches!(event, Event::Done(_));
        let _ = tx.send(event).await;

        // Тема — один запрос за всю жизнь чата, сразу после первого обмена.
        // Не вышло — остаётся заголовок из первого вопроса.
        if answered && chat.messages.len() == 2 {
            if let Some(title) = app.agent.title(&chat).await {
                chat.title = title.clone();
                match app.store.save(&chat) {
                    Ok(()) => {
                        let _ = tx.send(Event::Title(title)).await;
                    }
                    Err(error) => eprintln!("тема не сохранена: {error}"),
                }
            }
        }
    });

    let stream = ReceiverStream::new(rx).map(|event| {
        let (name, data) = match event {
            Event::Reasoning(delta) => ("reasoning", json!(delta)),
            Event::Content(delta) => ("content", json!(delta)),
            Event::Done(metrics) => ("done", json!(metrics)),
            Event::Title(title) => ("title", json!(title)),
            // Пересказ уезжает до начала стрима: страница показывает его
            // раньше, чем начнёт приходить сам ответ.
            Event::Summary(info) => ("summary", json!(info)),
            Event::SummaryError(error) => ("summary_error", json!({ "error": error })),
            // Переполнение контекста от прочих ошибок отличается только
            // текстом: страница по этому флагу вешает бейдж и красит полосу.
            Event::Error(error) => (
                "error",
                json!({ "message": error, "overflow": is_context_overflow(&error) }),
            ),
        };
        Ok::<_, Infallible>(SseEvent::default().event(name).data(data.to_string()))
    });
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}
