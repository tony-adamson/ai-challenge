//! w4d4 — веб-чат с явной моделью памяти агента, инструкциями пользователя,
//! состоянием задачи как конечным автоматом с условиями переходов и
//! инвариантами проекта.
//! Здесь только HTTP: маршруты, общее состояние и переклад событий агента в
//! SSE. Слои памяти, инструкции и инварианты лежат в файлах (`store.rs`), формат
//! API провайдеров, сборка запроса по слоям, валидатор и вся арифметика
//! токенов — в `agent.rs`.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
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
mod auth;
mod github;
mod mcp;
mod store;
mod summary;
mod watch;

use agent::{
    invariants_message, is_context_overflow, Agent, Event, Provider, Settings, Strategy, PERSONAS,
};
use store::{
    Category, Chat, Entry, Invariants, Kind, LongTerm, Stage, Store, Working, DATA_DIR, STALE_PLAN,
};

const DEFAULT_ADDR: &str = "127.0.0.1:8799";

pub struct App {
    pub agent: Agent,
    store: Store,
    /// Замок на правку памяти, инструкций и инвариантов. Все они правятся
    /// схемой «прочитать файл — изменить — записать», а `long_term.json` к тому
    /// же общий на все чаты: без сериализации второй писатель молча затирает
    /// первого. Замок один на всех, потому что «→ долговременная» и
    /// обновление после ответа трогают слои вместе.
    memory_lock: tokio::sync::Mutex<()>,
    mcp_lock: tokio::sync::Mutex<()>,
}

#[derive(Deserialize)]
struct AskRequest {
    text: String,
}

/// Куда переводить задачу руками: id этапа из `Stage::ALL`.
#[derive(Deserialize)]
struct StageRequest {
    to: String,
}

/// Что утверждает человек: шаги карточки плана ровно в том виде, в каком он
/// их прочитал. Сервер сверяет их с текущим черновиком.
#[derive(Deserialize)]
struct ApproveRequest {
    steps: Vec<String>,
}

/// Кривое тело запроса к ручкам задачи — читаемый отказ по-русски вместо
/// англоязычного текста axum: в эти ручки ходят и curl-ом, и ответ должен
/// сразу говорить, чего от него ждали.
fn bad_body(example: &str) -> Response {
    (StatusCode::BAD_REQUEST, format!("тело запроса не разобралось: нужен JSON вида {example}"))
        .into_response()
}

/// Индекс последнего сообщения, которое переезжает в ветку.
#[derive(Deserialize)]
struct BranchRequest {
    at: usize,
}

/// Новая запись долговременной памяти — руками из интерфейса.
#[derive(Deserialize)]
struct EntryRequest {
    kind: String,
    key: String,
    value: String,
}

/// Правка значения записи. Ключ и тип не меняются: сменить их — то же самое,
/// что завести другую запись.
#[derive(Deserialize)]
struct ValueRequest {
    value: String,
}

/// Куда переводить факт рабочей памяти: в долговременную запись выбранного типа.
#[derive(Deserialize)]
struct KindRequest {
    kind: String,
}

/// Новый инвариант из формы панели.
#[derive(Deserialize)]
struct InvariantRequest {
    category: String,
    rule: String,
    #[serde(default)]
    reason: String,
}

/// Правка инварианта: форма присылает категорию, правило и причину, тумблер —
/// только `enabled`. Чего нет, то не меняется.
#[derive(Deserialize)]
struct InvariantEdit {
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    rule: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

/// Инструкции целиком: одно текстовое поле, как его присылает окно настроек.
#[derive(Deserialize)]
struct InstructionsRequest {
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
    let mode = std::env::args().nth(1);
    if mode.as_deref() == Some("--mcp-server") {
        return watch::serve().await;
    }
    if mode.as_deref() == Some("--totp-init") {
        println!("{}", auth::init()?);
        return Ok(());
    }
    if mode.as_deref() == Some("--summary-now") {
        let _ = dotenvy::dotenv();
        let agent = Agent::new()?;
        let http = reqwest::Client::new();
        let telegram = summary::Telegram::from_env();
        std::fs::create_dir_all(DATA_DIR).map_err(|e| e.to_string())?;
        let sent = summary::cycle(&agent, &http, telegram.as_ref(), &mcp::watch_url(), summary::STATE_FILE).await?;
        println!("{}", if sent { summary::load(summary::STATE_FILE).digest.unwrap_or_default() }
            else { "Наблюдений нет — сводка не нужна".into() });
        return Ok(());
    }
    if matches!(mode.as_deref(), Some("--github-tools" | "--github-search")) {
        let query = if mode.as_deref() == Some("--github-search") {
            Some(std::env::args().nth(2).ok_or("Укажи поисковый запрос после --github-search")?)
        } else { None };
        println!("{}", serde_json::to_string_pretty(&mcp::github_cli(query).await?).map_err(|e| e.to_string())?);
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() == Some("--mcp-tools") {
        let catalog = mcp::discover(mcp::EXA_URL).await?;
        println!("{}", serde_json::to_string_pretty(&catalog).map_err(|e| e.to_string())?);
        return Ok(());
    }
    println!("AI Advent · w4d4 — исследователь: наблюдения за GitHub по расписанию через MCP");
    match dotenvy::dotenv() {
        Ok(path) => println!("  ✓ .env прочитан: {}", path.display()),
        Err(_) => println!("  · .env не найден, беру переменные окружения"),
    }
    let secret = std::env::var("TOTP_SECRET").ok().filter(|s| !s.trim().is_empty())
        .ok_or("TOTP_SECRET не задан: выполни `cargo run -- --totp-init` и добавь строку в корневой .env")?;

    let agent = Agent::new()?;
    for provider in Provider::ALL {
        match agent.is_available(provider) {
            true => println!("  ✓ провайдер: {}", provider.label()),
            false => println!("  · провайдер {} без ключа ({})", provider.label(), provider.key_env()),
        }
    }

    let store = Store::open(DATA_DIR)?;
    let auth = Arc::new(auth::Auth::new(&secret, auth::SESSIONS_FILE)?);
    println!("  ✓ вход по TOTP, сессии: {}", auth::SESSIONS_FILE);
    println!("  ✓ MCP-сервер наблюдений: {}", mcp::watch_url());
    match summary::Telegram::from_env() {
        Some(_) => println!("  ✓ сводка в Telegram раз в {} мин", summary::every().as_secs() / 60),
        None => println!("  · сводка раз в {} мин без Telegram (нет TELEGRAM_BOT_TOKEN/TELEGRAM_CHAT_ID)",
            summary::every().as_secs() / 60),
    }
    let long_term = store.long_term();
    println!("  ✓ чатов на диске: {} (папка {DATA_DIR}/)", store.list().len());
    println!(
        "  ✓ долговременная память: {} записей, {} ждут подтверждения",
        long_term.entries.len(),
        long_term.pending.len()
    );
    println!(
        "  ✓ инструкции: {} символов, файл {DATA_DIR}/instructions.md",
        store.instructions().trim().chars().count()
    );
    let invariants = store.invariants();
    println!(
        "  ✓ инвариантов: {} (включено {}), файл {DATA_DIR}/invariants.json",
        invariants.items.len(),
        invariants.enabled().len()
    );

    let app = Arc::new(App {
        agent, store,
        memory_lock: tokio::sync::Mutex::new(()),
        mcp_lock: tokio::sync::Mutex::new(()),
    });
    summary::spawn(app.clone());
    let router = Router::new()
        .route("/", get(|| async { Html(include_str!("../static/index.html")) }))
        .route("/static/colors_and_type.css", get(css))
        .route("/static/fonts/Manrope-VariableFont_wght.ttf", get(font))
        .route("/static/cerebras.svg", get(cerebras_logo))
        .route("/static/deepseek.svg", get(deepseek_logo))
        .route("/static/openrouter.svg", get(openrouter_logo))
        .route("/api/state", get(state))
        .route("/api/files/{name}", get(report_file))
        .route("/api/mcp/tools", get(mcp_tools))
        .route("/api/github/tools", get(github_tools))
        .route("/api/watches", get(watches))
        .route("/api/chats", post(create_chat))
        .route("/api/chats/{id}", get(open_chat))
        .route("/api/chats/{id}", delete(remove_chat))
        .route("/api/chats/{id}/settings", post(update_settings))
        .route("/api/chats/{id}/branch", post(branch_chat))
        .route("/api/chats/{id}/working", delete(clear_working))
        .route("/api/chats/{id}/working/{index}", delete(drop_working))
        .route("/api/chats/{id}/working/{index}/promote", post(promote_working))
        .route("/api/chats/{id}/task/transition", post(task_transition))
        .route("/api/chats/{id}/task/approve", post(task_approve))
        .route("/api/chats/{id}/task/pause", post(task_pause))
        .route("/api/chats/{id}/task/resume", post(task_resume))
        .route("/api/chats/{id}/ask", post(ask))
        .route("/api/long-term", post(add_entry))
        .route("/api/long-term", delete(clear_long_term))
        .route("/api/long-term/pending/{id}", post(confirm_pending))
        .route("/api/long-term/pending/{id}", delete(reject_pending))
        .route("/api/long-term/{id}", post(edit_entry))
        .route("/api/long-term/{id}", delete(drop_entry))
        .route("/api/invariants", post(add_invariant))
        .route("/api/invariants/{id}", post(edit_invariant))
        .route("/api/invariants/{id}", delete(drop_invariant))
        .route("/api/instructions", get(instructions))
        .route("/api/instructions", post(save_instructions))
        .with_state(app)
        .merge(auth::routes(auth.clone()))
        .layer(axum::middleware::from_fn_with_state(auth, auth::require));

    let addr = std::env::var("BIND_ADDR").ok().filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_ADDR.into());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("не удалось занять адрес {addr}: {e}"))?;
    let url = format!("http://{addr}");
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

/// Скачивание отчёта из `data/reports/`: имя проходит через общую
/// проверку `watch::report_name` и должно совпасть побайтово — без `.md`
/// отдаётся 400. Путь всегда `<dir>/<report_name>`: выйти за папку нельзя,
/// потому что в имени только латиница в нижнем регистре, цифры, `-`, `_` и `.md`.
fn report_response(dir: &std::path::Path, name: &str) -> Response {
    let file = match crate::watch::report_name(name) {
        Ok(file) if file == name => file,
        _ => return (StatusCode::BAD_REQUEST, "неверное имя файла").into_response(),
    };
    let body = match std::fs::read(dir.join(&file)) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (StatusCode::NOT_FOUND, "файл не найден").into_response()
        }
        Err(error) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("не прочитать {file}: {error}")).into_response(),
    };
    let disposition = format!("attachment; filename=\"{file}\"");
    axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/markdown; charset=utf-8")
        .header(header::CONTENT_DISPOSITION, disposition)
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|error| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("не отдать {file}: {error}")).into_response()
        })
}

async fn report_file(Path(name): Path<String>) -> Response {
    report_response(std::path::Path::new(crate::watch::REPORTS_DIR), &name)
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "чат не найден").into_response()
}

async fn github_tools(State(app): State<Arc<App>>) -> Response {
    let Ok(_guard) = app.mcp_lock.try_lock() else {
        return (StatusCode::CONFLICT, "Подключение MCP уже выполняется").into_response();
    };
    match mcp::github_cli(None).await {
        Ok(tools) => Json(json!({"tools":tools})).into_response(),
        Err(error) => (StatusCode::BAD_GATEWAY, error).into_response(),
    }
}

/// Панель «Наблюдения»: список с MCP-сервера и последний дайджест из файла.
/// Сервер недоступен — дайджест всё равно показываем, а ошибку отдаём полем.
async fn watches() -> Json<Value> {
    let listed = async {
        let client = mcp::connect(&mcp::watch_url()).await?;
        let result = mcp::call(&client, "watch_list", json!({})).await;
        mcp::close(client).await;
        result
    }.await;
    let state = summary::load(summary::STATE_FILE);
    let digest = json!({"text": state.digest, "sent_at": state.sent_at, "status": state.status});
    match listed {
        Ok(list) => Json(json!({"watches": list["watches"], "digest": digest})),
        Err(error) => Json(json!({"watches": [], "error": error, "digest": digest})),
    }
}

async fn mcp_tools(State(app): State<Arc<App>>) -> Response {
    let Ok(_guard) = app.mcp_lock.try_lock() else {
        return (StatusCode::CONFLICT, "Подключение к Exa уже выполняется").into_response();
    };
    match mcp::discover(mcp::EXA_URL).await {
        Ok(catalog) => Json(catalog).into_response(),
        Err(error) => (StatusCode::BAD_GATEWAY, error).into_response(),
    }
}

/// Всё, что нужно странице при загрузке: список чатов, открытый чат,
/// справочник провайдеров с их моделями и уровнями рассуждения, список
/// личностей с шаблонами промптов и долговременная память — она одна на всё
/// приложение, поэтому приезжает вместе с состоянием, а не с чатом.
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
            // Проверка инвариантов — такие же деньги этого чата, как и ответы.
            let total_cost_usd: f64 = metrics
                .map(|m| m.cost_usd.unwrap_or(0.0) + m.check.map_or(0.0, |c| c.cost)
                    + m.tool_selection.map_or(0.0, |c| c.cost))
                .sum();
            json!({
                "id": c.id,
                "title": c.title,
                "updated_at": c.updated_at,
                "provider": c.settings.provider,
                "model": c.settings.model,
                "last_prompt_tokens": last_prompt_tokens,
                "total_cost_usd": total_cost_usd,
                // Ветка показывается под своим родителем: список уже приходит
                // в нужном порядке (`order_with_branches`), странице остаётся
                // отступ и подпись.
                "parent": c.parent,
                "branch_from": c.branch_from,
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

    let invariants = app.store.invariants();
    Json(json!({
        "active_chat": app.store.active(),
        "chats": chats,
        "providers": providers,
        "personas": PERSONAS.iter()
            .map(|p| json!({ "id": p.id, "name": p.name, "prompt": p.prompt }))
            .collect::<Vec<_>>(),
        // Названия стратегий живут в одном месте — в `agent.rs`: страница
        // строит по ним селектор и подписи, а не повторяет список у себя.
        "strategies": Strategy::ALL.iter()
            .map(|s| json!({ "id": s, "label": s.label() }))
            .collect::<Vec<_>>(),
        // Этапы задачи, разрешённые переходы и подписи условий — из кода, а
        // не из копии на странице: в этом весь смысл автомата. Панель рисует
        // по этому списку схему; решает, пускать ли, всё равно сервер.
        "stages": Stage::ALL.iter()
            .map(|s| json!({
                "id": s.id(),
                "label": s.label(),
                "rule": s.rule(),
                "allowed": s.allowed().iter().map(|a| a.id()).collect::<Vec<_>>(),
                "gate": s.gate(),
            }))
            .collect::<Vec<_>>(),
        // Типы долговременных записей — по той же причине в одном месте.
        "kinds": Kind::ALL.iter()
            .map(|k| json!({ "id": k.id(), "label": k.label(), "group": k.group() }))
            .collect::<Vec<_>>(),
        "long_term": app.store.long_term(),
        // Инварианты общие на приложение, как и долговременная. Категории — из
        // кода; текст блока — ровно тот, что уйдёт в запрос: страница считает
        // по нему символы и показывает его, не повторяя формулировки у себя.
        "invariants": invariants.items,
        "invariants_block": invariants_message(&invariants.items).map(|m| m.content),
        "categories": Category::ALL.iter()
            .map(|c| json!({ "id": c.id(), "label": c.label() }))
            .collect::<Vec<_>>(),
        // Инструкции общие на приложение, как и долговременная память.
        "instructions": app.store.instructions(),
    }))
}

/// Новый чат наследует настройки открытого: менять провайдера и промпт
/// заново на каждый чат — лишняя работа. Нет открытого — умолчания, и с ними
/// первая личность из списка.
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
    opened(&app, chat)
}

/// Открытие чата заодно двигает указатель: после F5 страница вернётся туда,
/// где её оставили.
async fn open_chat(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let Some(chat) = app.store.load(&id) else { return not_found() };
    if let Err(error) = app.store.set_active(&chat.id) {
        eprintln!("{error}");
    }
    opened(&app, chat)
}

/// Открытый чат вместе с его рабочей памятью: они лежат в разных файлах, но
/// странице нужны разом. Долговременная сюда не идёт — она общая и приезжает
/// с `/api/state`.
fn opened(app: &App, chat: Chat) -> Response {
    let working = app.store.working(&chat.id);
    Json(json!({ "chat": chat, "working": working })).into_response()
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

/// Ветка от сообщения: копия чата по это сообщение включительно. Новая ветка
/// сразу становится открытой — иначе после ветвления пришлось бы искать её в
/// списке руками. Ответ той же формы, что и у создания чата.
async fn branch_chat(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(request): Json<BranchRequest>,
) -> Response {
    let Some(parent) = app.store.load(&id) else { return not_found() };
    let branch = match parent.branch(request.at) {
        Ok(branch) => branch,
        Err(error) => return (StatusCode::BAD_REQUEST, error).into_response(),
    };
    if let Err(error) = app.store.save(&branch) {
        return (StatusCode::INTERNAL_SERVER_ERROR, error).into_response();
    }
    // Рабочая память лежит в своём файле, поэтому копируется отдельно:
    // продолжение того же разговора должно помнить ту же задачу. Чтение
    // родителя и запись ветки — под общим замком памяти, как и всё остальное.
    {
        let _guard = app.memory_lock.lock().await;
        if let Err(error) = app.store.branch_working(&parent.id, &branch.id) {
            eprintln!("рабочая память ветки не сохранена: {error}");
        }
    }
    if let Err(error) = app.store.set_active(&branch.id) {
        eprintln!("{error}");
    }
    opened(&app, branch)
}

/// Ручная правка рабочей памяти: факт можно выбросить, если модель запомнила
/// лишнее. Редактирования значений здесь нет — проще сказать модели новое.
async fn drop_working(
    State(app): State<Arc<App>>,
    Path((id, index)): Path<(String, usize)>,
) -> Response {
    if app.store.load(&id).is_none() {
        return not_found();
    }
    let _guard = app.memory_lock.lock().await;
    let mut working = app.store.working(&id);
    if index >= working.facts.len() {
        return (StatusCode::BAD_REQUEST, "такого факта нет").into_response();
    }
    working.facts.remove(index);
    save_working(&app, &id, &working)
}

async fn clear_working(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    if app.store.load(&id).is_none() {
        return not_found();
    }
    let _guard = app.memory_lock.lock().await;
    let mut working = app.store.working(&id);
    working.facts.clear();
    save_working(&app, &id, &working)
}

/// Перевод факта из рабочей памяти в долговременную: человек решил, что это
/// переживёт текущую задачу. Из рабочей факт при этом уходит — одна и та же
/// строка в двух слоях означала бы, что слои ничего не делят.
async fn promote_working(
    State(app): State<Arc<App>>,
    Path((id, index)): Path<(String, usize)>,
    Json(request): Json<KindRequest>,
) -> Response {
    if app.store.load(&id).is_none() {
        return not_found();
    }
    let Some(kind) = Kind::from_id(&request.kind) else {
        return (StatusCode::BAD_REQUEST, "неизвестный тип записи").into_response();
    };
    let _guard = app.memory_lock.lock().await;
    let mut working = app.store.working(&id);
    let mut long_term = app.store.long_term();
    // Дубль и отсутствие факта проверяет сам слой, до того как что-то
    // сдвинулось: иначе факт исчез бы из рабочей памяти, не появившись в
    // долговременной.
    if let Err(error) = long_term.promote(kind, &mut working, index) {
        return (StatusCode::BAD_REQUEST, error).into_response();
    }
    if let Err(error) = app.store.save_long_term(&long_term) {
        return (StatusCode::INTERNAL_SERVER_ERROR, error).into_response();
    }
    if let Err(error) = app.store.save_working(&id, &working) {
        return (StatusCode::INTERNAL_SERVER_ERROR, error).into_response();
    }
    Json(json!({ "working": working, "long_term": long_term })).into_response()
}

/// Накопительные счётчики памяти при правке не трогаем: обновления уже были
/// оплачены, и обнулять их из-за выброшенной строки было бы враньём.
fn save_working(app: &App, id: &str, working: &Working) -> Response {
    match app.store.save_working(id, working) {
        Ok(()) => Json(working).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}

/// Ручной перевод этапа. Таблица и условия те же, что у предложений агента
/// (`Task::transition`), меняется только подпись в журнале: `human` вместо
/// `agent`. Узлы схемы в панели кликабельны все, включая запрещённые, —
/// отказ приходит отсюда: 400 с читаемой причиной, а попытка остаётся в
/// журнале, поэтому рабочая память сохраняется и при отказе.
async fn task_transition(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    request: Result<Json<StageRequest>, JsonRejection>,
) -> Response {
    if app.store.load(&id).is_none() {
        return not_found();
    }
    let Ok(Json(request)) = request else { return bad_body(r#"{"to": "execution"}"#) };
    let Some(to) = Stage::from_id(&request.to) else {
        return (StatusCode::BAD_REQUEST, "неизвестный этап").into_response();
    };
    let _guard = app.memory_lock.lock().await;
    let mut working = app.store.working(&id);
    let result = working.task.transition(to, "human", "кнопка в панели");
    task_changed(&app, &id, &working, result)
}

/// «Утвердить и выполнять» под карточкой плана: отметка утверждения и
/// переход в выполнение одним действием. Фраза в чате план не утверждает —
/// только этот маршрут. В теле — шаги карточки: черновик успел смениться —
/// 409 «план изменился», утверждается ровно то, что человек прочитал. Плана
/// нет — отказ условием. Оба отказа остаются в журнале.
async fn task_approve(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    request: Result<Json<ApproveRequest>, JsonRejection>,
) -> Response {
    if app.store.load(&id).is_none() {
        return not_found();
    }
    let Ok(Json(request)) = request else {
        return bad_body(r#"{"steps": ["шаг 1", "шаг 2"]} — шаги плана с карточки"#);
    };
    let _guard = app.memory_lock.lock().await;
    let mut working = app.store.working(&id);
    let result = working.task.approve(&request.steps);
    task_changed(&app, &id, &working, result)
}

/// Ответ маршрутов перехода: рабочая память сохраняется в обоих случаях —
/// отказ пишется в журнал, и журнал должен пережить перезапуск. Отказ
/// возвращается текстом, страница показывает его тостом и перечитывает чат.
fn task_changed(app: &App, id: &str, working: &Working, result: Result<(), String>) -> Response {
    let saved = save_working(app, id, working);
    match result {
        Ok(()) => saved,
        // Устаревшая карточка — конфликт версий, а не кривой запрос.
        Err(error) if error == STALE_PLAN => (StatusCode::CONFLICT, error).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, error).into_response(),
    }
}

/// Пауза: этап, план, шаг и ожидаемое действие остаются на диске как есть.
/// Пока она стоит, новые сообщения в чат не принимаются (см. `ask`), а после
/// перезапуска сервера задача продолжается с того же места.
async fn task_pause(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    if app.store.load(&id).is_none() {
        return not_found();
    }
    let _guard = app.memory_lock.lock().await;
    let mut working = app.store.working(&id);
    working.task.pause();
    save_working(&app, &id, &working)
}

/// Продолжение: снимаем паузу и ставим флаг на один следующий запрос — в блок
/// задачи добавится строка «продолжай с текущего шага, не пересказывай».
async fn task_resume(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    if app.store.load(&id).is_none() {
        return not_found();
    }
    let _guard = app.memory_lock.lock().await;
    let mut working = app.store.working(&id);
    working.task.resume();
    save_working(&app, &id, &working)
}

/// Правка долговременной памяти — целиком руками. Запись добавляют, её
/// значение правят, её удаляют; модель сюда попадает только через `pending` и
/// только с подтверждением.
async fn add_entry(State(app): State<Arc<App>>, Json(request): Json<EntryRequest>) -> Response {
    let Some(kind) = Kind::from_id(&request.kind) else {
        return (StatusCode::BAD_REQUEST, "неизвестный тип записи").into_response();
    };
    let (key, value) = (request.key.trim().to_string(), request.value.trim().to_string());
    if key.is_empty() || value.is_empty() {
        return (StatusCode::BAD_REQUEST, "нужны и ключ, и значение").into_response();
    }
    let _guard = app.memory_lock.lock().await;
    let mut long_term = app.store.long_term();
    if long_term.knows(kind, &key) {
        return (StatusCode::BAD_REQUEST, "такая запись уже есть").into_response();
    }
    long_term.entries.push(Entry::new(kind, key, value));
    save_long_term(&app, long_term)
}

async fn edit_entry(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(request): Json<ValueRequest>,
) -> Response {
    let value = request.value.trim().to_string();
    if value.is_empty() {
        return (StatusCode::BAD_REQUEST, "пустое значение").into_response();
    }
    let _guard = app.memory_lock.lock().await;
    let mut long_term = app.store.long_term();
    let Some(entry) = long_term.entries.iter_mut().find(|e| e.id == id) else {
        return (StatusCode::NOT_FOUND, "такой записи нет").into_response();
    };
    entry.value = value;
    save_long_term(&app, long_term)
}

async fn drop_entry(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let _guard = app.memory_lock.lock().await;
    let mut long_term = app.store.long_term();
    long_term.entries.retain(|e| e.id != id);
    save_long_term(&app, long_term)
}

async fn clear_long_term(State(app): State<Arc<App>>) -> Response {
    let _guard = app.memory_lock.lock().await;
    let mut long_term = app.store.long_term();
    long_term.entries.clear();
    save_long_term(&app, long_term)
}

/// «Запомнить»: предложение модели становится записью. Время создания при этом
/// не подменяется — важно, когда факт прозвучал, а не когда его подтвердили.
async fn confirm_pending(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let _guard = app.memory_lock.lock().await;
    let mut long_term = app.store.long_term();
    let Some(entry) = long_term.take_pending(&id) else {
        return (StatusCode::NOT_FOUND, "такого предложения нет").into_response();
    };
    long_term.entries.push(entry);
    save_long_term(&app, long_term)
}

/// «Нет»: предложение выбрасывается. Второй раз то же самое модель предложит
/// только в новом разговоре — в этом дубли отсеиваются по `pending`.
async fn reject_pending(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let _guard = app.memory_lock.lock().await;
    let mut long_term = app.store.long_term();
    long_term.pending.retain(|e| e.id != id);
    save_long_term(&app, long_term)
}

fn save_long_term(app: &App, long_term: LongTerm) -> Response {
    match app.store.save_long_term(&long_term) {
        Ok(()) => Json(long_term).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}

/// Инварианты пишет только человек: добавить, поправить, выключить, удалить.
/// Служебных вызовов, которые бы их предлагали, нет. Ответ — весь набор: он
/// маленький, и странице проще заменить его целиком.
async fn add_invariant(
    State(app): State<Arc<App>>,
    Json(request): Json<InvariantRequest>,
) -> Response {
    let Some(category) = Category::from_id(&request.category) else {
        return (StatusCode::BAD_REQUEST, "неизвестная категория инварианта").into_response();
    };
    let _guard = app.memory_lock.lock().await;
    let mut invariants = app.store.invariants();
    if let Err(error) = invariants.add(category, &request.rule, &request.reason) {
        return (StatusCode::BAD_REQUEST, error).into_response();
    }
    save_invariants(&app, invariants)
}

async fn edit_invariant(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(request): Json<InvariantEdit>,
) -> Response {
    let category = match request.category.as_deref() {
        None => None,
        Some(value) => match Category::from_id(value) {
            Some(category) => Some(category),
            None => {
                return (StatusCode::BAD_REQUEST, "неизвестная категория инварианта").into_response()
            }
        },
    };
    let _guard = app.memory_lock.lock().await;
    let mut invariants = app.store.invariants();
    let edited = invariants.edit(
        &id,
        category,
        request.rule.as_deref(),
        request.reason.as_deref(),
        request.enabled,
    );
    if let Err(error) = edited {
        let status = if invariants.items.iter().any(|i| i.id == id) {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::NOT_FOUND
        };
        return (status, error).into_response();
    }
    save_invariants(&app, invariants)
}

async fn drop_invariant(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let _guard = app.memory_lock.lock().await;
    let mut invariants = app.store.invariants();
    if let Err(error) = invariants.remove(&id) {
        return (StatusCode::NOT_FOUND, error).into_response();
    }
    save_invariants(&app, invariants)
}

/// Вместе с набором уходит и текст блока — тот же, что в `/api/state`.
fn save_invariants(app: &App, invariants: Invariants) -> Response {
    match app.store.save_invariants(&invariants) {
        Ok(()) => Json(json!({
            "invariants": invariants.items,
            "invariants_block": invariants_message(&invariants.items).map(|m| m.content),
        }))
        .into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}

/// Инструкции пишет только человек — в окне настроек. Пустой текст — не
/// ошибка: слоя в запросе тогда просто нет.
async fn instructions(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!({ "text": app.store.instructions() }))
}

async fn save_instructions(
    State(app): State<Arc<App>>,
    Json(request): Json<InstructionsRequest>,
) -> Response {
    let _guard = app.memory_lock.lock().await;
    match app.store.save_instructions(&request.text) {
        Ok(()) => Json(json!({ "text": request.text })).into_response(),
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
    // Оба слоя читаются с диска ровно перед запросом: между ходами их могли
    // поправить руками, и запрос должен уйти с тем, что лежит сейчас. Это
    // только вход промпта — сливать ответ памяти будем по свежим копиям.
    let long_term = app.store.long_term();
    let working = app.store.working(&id);
    // Инструкции читаются там же и по той же причине: их могли поправить
    // между ходами, и запрос должен уйти с тем, что написано сейчас.
    let instructions = app.store.instructions();
    // Инварианты — тоже с диска перед запросом: их могли выключить или
    // поправить в панели, и проверять надо по тому, что задано сейчас.
    let invariants = app.store.invariants();

    // Пауза — замок: сообщение не уходит модели и не пишется в историю.
    // Продолжить задачу можно только кнопкой, и тогда следующий запрос уйдёт
    // со строкой «не пересказывай заново».
    if working.task.paused_at.is_some() {
        return (StatusCode::CONFLICT, "Задача на паузе — нажми «Продолжить»").into_response();
    }

    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(async move {
        // Занятость чата держится до конца служебного вызова памяти, поэтому
        // guard живёт весь этот блок: иначе следующий вопрос успел бы прочитать
        // рабочую память раньше, чем в неё легло обновление этого хода.
        let _busy = match app.agent.reserve(&chat.id) {
            Ok(guard) => guard,
            Err(error) => {
                let _ = tx.send(Event::Error(error)).await;
                return;
            }
        };
        // Отклонённый запрос агент дописывает в историю записью `error`, и её
        // тоже надо сохранить. Отказы до самого запроса (пустой текст, кривые
        // настройки) историю не трогают — такой чат перезаписывать незачем.
        let before = chat.messages.len();
        let event =
            match app
                .agent
                .ask(&mut chat, &invariants.items, &instructions, &long_term, &working, &request.text, &tx)
                .await
            {
            Ok(metrics) => match app.store.save(&chat) {
                Ok(()) => Event::Done(Box::new(metrics)),
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

        // Два изменения задачи делает сам ответ, до обновления памяти. Флаг
        // «только что продолжили после паузы» действует ровно на один запрос —
        // он уже ушёл. А блок ```plan в ответе становится черновиком плана —
        // ровно тот текст, что прочитал человек; служебный вызов план не
        // пишет. Не принятый блок (ответ красный, план уже утверждён, этап не
        // тот) получает пометку у сообщения: лента покажет «не принят», а не
        // «заменён».
        // Рабочая перечитывается под замком, и служебный вызов памяти
        // получает уже её — с планом этого хода.
        let mut working = working;
        if answered {
            let _guard = app.memory_lock.lock().await;
            working = app.store.working(&chat.id);
            let resumed = working.task.resumed_from.take().is_some();
            // Красный ответ план не даёт (`plan_offer`): утвердить можно только
            // то, что прошло проверку или не было проверено из-за осечки.
            let offered = chat
                .messages
                .last()
                .and_then(agent::plan_offer)
                .map(|found| found.and_then(|steps| working.task.offer_plan(steps)));
            let planned = matches!(offered, Some(Ok(())));
            if let (Some(Err(note)), Some(last)) = (offered, chat.messages.last_mut()) {
                last.plan_note = Some(note);
                if let Err(error) = app.store.save(&chat) {
                    eprintln!("пометка плана не сохранена: {error}");
                }
            }
            if resumed || planned {
                if let Err(error) = app.store.save_working(&chat.id, &working) {
                    eprintln!("состояние задачи не сохранено: {error}");
                }
                let _ = tx.send(Event::Task(working.task.clone())).await;
            }
        }

        // Память обновляется после ответа, а не до: так в неё попадают и
        // решения самой модели, а человек не ждёт лишнего запроса перед
        // стримом. Один ход — одно обновление на оба слоя; осечка памяти уже
        // отданный ответ не отменяет. Служебный вызов привязан к рабочей
        // памяти: выключили её — платить за раскладку по слоям незачем.
        if answered && chat.settings.layers.working {
            match app.agent.update_memory(&chat, &long_term, &working).await {
                Ok(reply) => {
                    // Слои перечитываются с диска здесь, под замком, а не
                    // берутся из копии, прочитанной перед стримом: пока шёл
                    // ответ, их мог поправить человек или соседний чат.
                    let _guard = app.memory_lock.lock().await;
                    let mut long_term = app.store.long_term();
                    let mut working = app.store.working(&chat.id);
                    let info = agent::apply_memory(reply, &mut long_term, &mut working);
                    if let Err(error) = app.store.save_working(&chat.id, &working) {
                        eprintln!("рабочая память не сохранена: {error}");
                    }
                    // Долговременную трогаем только если в очередь что-то
                    // добавилось: лишняя перезапись общего файла ни к чему.
                    if info.added > 0 {
                        if let Err(error) = app.store.save_long_term(&long_term) {
                            eprintln!("долговременная память не сохранена: {error}");
                        }
                    }
                    drop(_guard);
                    let _ = tx.send(Event::Memory(info)).await;
                }
                Err(error) => {
                    let _ = tx.send(Event::MemoryError(error)).await;
                }
            }
        }

        // Тема — один запрос за всю жизнь чата, сразу после первого обмена.
        // Не вышло — остаётся заголовок из первого вопроса. У ветки темы не
        // просим вовсе: её заголовок говорит, откуда она отпочковалась.
        if answered && chat.parent.is_none() && chat.messages.len() == 2 {
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
            Event::Tool(trace) => ("tool", json!(trace)),
            Event::Content(delta) => ("content", json!(delta)),
            Event::Done(metrics) => ("done", json!(metrics)),
            Event::Title(title) => ("title", json!(title)),
            // Пересказ уезжает до начала стрима: страница показывает его
            // раньше, чем начнёт приходить сам ответ.
            Event::Summary(info) => ("summary", json!(info)),
            Event::SummaryError(error) => ("summary_error", json!({ "error": error })),
            // Память, наоборот, приезжает после `done`: она собирается по уже
            // готовому ответу. Поток закрывается только после неё.
            Event::Memory(info) => ("memory", json!(info)),
            Event::Task(task) => ("task", json!(task)),
            Event::MemoryError(error) => ("memory_error", json!({ "error": error })),
            // Пока ответ проверяется, страница показывает лоадер с этапом, а
            // не текст: непроверенное наружу не уходит.
            Event::Phase { phase, ids } => ("phase", json!({ "phase": phase, "ids": ids })),
            Event::Check(verdict) => ("check", json!(verdict)),
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
