//! w3d2 — веб-чат с явной моделью памяти агента и профилем пользователя.
//! Здесь только HTTP: маршруты, общее состояние и переклад событий агента в
//! SSE. Слои памяти и профили лежат в файлах (`store.rs`), формат API
//! провайдеров, сборка запроса по слоям и вся арифметика токенов — в
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

use agent::{is_context_overflow, persona, Agent, Event, Provider, Settings, Strategy, PERSONAS};
use store::{Chat, Entry, Kind, LongTerm, Profile, Store, Working, DATA_DIR};

const ADDR: &str = "127.0.0.1:8796";

struct App {
    agent: Agent,
    store: Store,
    /// Замок на правку памяти и профиля. Все они правятся схемой «прочитать
    /// файл — изменить — записать», а `long_term.json` и файл профиля к тому
    /// же общие на все чаты: без сериализации второй писатель молча затирает
    /// первого. Замок один на всех, потому что «→ долговременная» и
    /// обновление после ответа трогают их вместе.
    memory_lock: tokio::sync::Mutex<()>,
}

#[derive(Deserialize)]
struct AskRequest {
    text: String,
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

/// Куда переводить факт рабочей памяти: в заметку профиля
/// (`PROFILE_NOTE_TARGET`) или в долговременную запись выбранного типа.
#[derive(Deserialize)]
struct KindRequest {
    kind: String,
}

/// Поля профиля, как их присылает форма. Заметки и очередь предложений сюда
/// не входят: их правят кнопками, а не формой.
#[derive(Deserialize)]
struct ProfileRequest {
    name: String,
    address: String,
    style: String,
    format: String,
    constraints: String,
    context: String,
    persona: String,
    steps: String,
}

/// Значение «типа» в кнопке «→ долговременная», означающее заметку профиля.
/// Тип `profile` в долговременной памяти дня 11 был третьим `Kind`; теперь
/// это другой слой, и путать их нечем.
const PROFILE_NOTE_TARGET: &str = "profile_note";

#[tokio::main]
async fn main() {
    if let Err(err) = start().await {
        eprintln!("Ошибка: {err}");
        std::process::exit(1);
    }
}

async fn start() -> Result<(), String> {
    println!("AI Advent · w3d2 — персонализация агента");
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
    let long_term = store.long_term();
    println!("  ✓ чатов на диске: {} (папка {DATA_DIR}/)", store.list().len());
    println!(
        "  ✓ долговременная память: {} записей, {} ждут подтверждения",
        long_term.entries.len(),
        long_term.pending.len()
    );
    let profiles = store.profiles();
    let active = store.active_profile_or_empty();
    println!(
        "  ✓ профилей: {} (папка {DATA_DIR}/profiles), активный: {}",
        profiles.len(),
        if active.name.is_empty() { "нет".to_string() } else { active.name.clone() }
    );

    let app = Arc::new(App { agent, store, memory_lock: tokio::sync::Mutex::new(()) });
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
        .route("/api/chats/{id}/branch", post(branch_chat))
        .route("/api/chats/{id}/working", delete(clear_working))
        .route("/api/chats/{id}/working/{index}", delete(drop_working))
        .route("/api/chats/{id}/working/{index}/promote", post(promote_working))
        .route("/api/chats/{id}/ask", post(ask))
        .route("/api/long-term", post(add_entry))
        .route("/api/long-term", delete(clear_long_term))
        .route("/api/long-term/pending/{id}", post(confirm_pending))
        .route("/api/long-term/pending/{id}", delete(reject_pending))
        .route("/api/long-term/{id}", post(edit_entry))
        .route("/api/long-term/{id}", delete(drop_entry))
        .route("/api/profiles", post(create_profile))
        .route("/api/profiles/{id}", post(save_profile))
        .route("/api/profiles/{id}", delete(remove_profile))
        .route("/api/profiles/{id}/active", post(choose_profile))
        .route("/api/profiles/{id}/pending/{note}", post(confirm_note))
        .route("/api/profiles/{id}/pending/{note}", delete(reject_note))
        .route("/api/profiles/{id}/notes/{note}", delete(drop_note))
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
            let total_cost_usd: f64 = metrics.filter_map(|m| m.cost_usd).sum();
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
        // Типы долговременных записей — по той же причине в одном месте.
        "kinds": Kind::ALL.iter()
            .map(|k| json!({ "id": k.id(), "label": k.label(), "group": k.group() }))
            .collect::<Vec<_>>(),
        "long_term": app.store.long_term(),
        // Профили целиком: их немного, а странице нужны и поля формы, и
        // заметки с очередью предложений.
        "profiles": app.store.profiles(),
        "active_profile": app.store.active_profile(),
        // Значение «типа» для кнопки «→ долговременная», которое означает
        // заметку профиля: страница не должна повторять эту строку у себя.
        "profile_note_target": PROFILE_NOTE_TARGET,
    }))
}

/// Новый чат наследует настройки открытого: менять провайдера и промпт
/// заново на каждый чат — лишняя работа. Личность при этом берётся из
/// активного профиля, если он её назвал: профиль «Первокурсник» и так знает,
/// что разговор будет про роботов.
async fn create_chat(State(app): State<Arc<App>>) -> Response {
    let mut settings = app
        .store
        .active()
        .and_then(|id| app.store.load(&id))
        .map(|chat| chat.settings)
        .unwrap_or_else(|| app.agent.default_settings());
    if let Some(wanted) = persona(&app.store.active_profile_or_empty().persona) {
        settings.persona = wanted.id.to_string();
        settings.system_prompt = wanted.prompt.to_string();
    }

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
    if request.kind == PROFILE_NOTE_TARGET {
        return promote_to_profile(&app, &id, index).await;
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

/// Тот же перевод, только в заметку активного профиля: человек решил, что это
/// не про задачу, а про него самого. Дубль по ключу — отказ до того, как факт
/// уйдёт из рабочей памяти.
async fn promote_to_profile(app: &App, chat: &str, index: usize) -> Response {
    let _guard = app.memory_lock.lock().await;
    let mut working = app.store.working(chat);
    let Some(mut profile) = app.store.active_profile().and_then(|id| app.store.profile(&id)) else {
        return (StatusCode::BAD_REQUEST, "профиль не выбран").into_response();
    };
    let Some(fact) = working.facts.get(index) else {
        return (StatusCode::BAD_REQUEST, "такого факта нет").into_response();
    };
    if profile.knows(&fact.key) {
        return (StatusCode::BAD_REQUEST, "такая заметка уже есть").into_response();
    }
    let fact = working.facts.remove(index);
    profile.notes.push(store::Note::new(fact.key, fact.value));
    if let Err(error) = app.store.save_profile(&profile) {
        return (StatusCode::INTERNAL_SERVER_ERROR, error).into_response();
    }
    if let Err(error) = app.store.save_working(chat, &working) {
        return (StatusCode::INTERNAL_SERVER_ERROR, error).into_response();
    }
    Json(json!({ "working": working, "profile": profile })).into_response()
}

/// Накопительные счётчики памяти при правке не трогаем: обновления уже были
/// оплачены, и обнулять их из-за выброшенной строки было бы враньём.
fn save_working(app: &App, id: &str, working: &Working) -> Response {
    match app.store.save_working(id, working) {
        Ok(()) => Json(working).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
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

/// Новый профиль сразу становится активным: «новый» нажимают, чтобы им
/// пользоваться, а поля заполняются в той же модалке следом.
async fn create_profile(State(app): State<Arc<App>>) -> Response {
    let _guard = app.memory_lock.lock().await;
    let profile = Profile::new("Новый профиль".to_string());
    if let Err(error) = app.store.save_profile(&profile) {
        return (StatusCode::INTERNAL_SERVER_ERROR, error).into_response();
    }
    if let Err(error) = app.store.set_active_profile(&profile.id) {
        eprintln!("{error}");
    }
    Json(profile).into_response()
}

/// Правка полей профиля из формы. Заметки и очередь предложений при этом
/// остаются как были: форма их не показывает и затирать их ей нечем.
async fn save_profile(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(request): Json<ProfileRequest>,
) -> Response {
    let name = request.name.trim().to_string();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "у профиля должно быть название").into_response();
    }
    if !request.persona.is_empty() && persona(&request.persona).is_none() {
        return (StatusCode::BAD_REQUEST, "неизвестная личность").into_response();
    }
    let _guard = app.memory_lock.lock().await;
    let Some(stored) = app.store.profile(&id) else {
        return (StatusCode::NOT_FOUND, "такого профиля нет").into_response();
    };
    let profile = Profile {
        name,
        address: request.address,
        style: request.style,
        format: request.format,
        constraints: request.constraints,
        context: request.context,
        persona: request.persona,
        steps: request.steps,
        ..stored
    };
    match app.store.save_profile(&profile) {
        Ok(()) => Json(profile).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}

/// Удаление профиля. Последний не удаляется — отказ приходит читаемой
/// строкой, её и показывает страница.
async fn remove_profile(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let _guard = app.memory_lock.lock().await;
    match app.store.delete_profile(&id) {
        Ok(()) => Json(json!({ "active_profile": app.store.active_profile() })).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, error).into_response(),
    }
}

/// Переключение профиля. Действует со следующего запроса в любом чате:
/// профиль общий, как и долговременная память.
async fn choose_profile(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let Some(profile) = app.store.profile(&id) else {
        return (StatusCode::NOT_FOUND, "такого профиля нет").into_response();
    };
    match app.store.set_active_profile(&profile.id) {
        Ok(()) => Json(profile).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}

/// «Запомнить»: предложение модели становится заметкой профиля.
async fn confirm_note(
    State(app): State<Arc<App>>,
    Path((id, note)): Path<(String, String)>,
) -> Response {
    edit_profile(&app, &id, |profile| match profile.take_pending(&note) {
        Some(note) => {
            profile.notes.push(note);
            Ok(())
        }
        None => Err("такого предложения нет".to_string()),
    })
    .await
}

/// «Нет»: предложение выбрасывается. Второй раз то же самое модель предложит
/// только в новом разговоре — в этом дубли отсеиваются по очереди.
async fn reject_note(
    State(app): State<Arc<App>>,
    Path((id, note)): Path<(String, String)>,
) -> Response {
    edit_profile(&app, &id, |profile| {
        profile.pending.retain(|n| n.id != note);
        Ok(())
    })
    .await
}

async fn drop_note(
    State(app): State<Arc<App>>,
    Path((id, note)): Path<(String, String)>,
) -> Response {
    edit_profile(&app, &id, |profile| {
        profile.notes.retain(|n| n.id != note);
        Ok(())
    })
    .await
}

/// Правка профиля по схеме «прочитать — изменить — записать» под общим
/// замком: пока страница жала кнопку, профиль мог обновить служебный вызов
/// соседнего чата. Ответ — профиль целиком: он небольшой, а склеивать его на
/// странице по кусочкам значило бы держать вторую правду.
async fn edit_profile(
    app: &App,
    id: &str,
    change: impl FnOnce(&mut Profile) -> Result<(), String>,
) -> Response {
    let _guard = app.memory_lock.lock().await;
    let Some(mut profile) = app.store.profile(id) else {
        return (StatusCode::NOT_FOUND, "такого профиля нет").into_response();
    };
    if let Err(error) = change(&mut profile) {
        return (StatusCode::NOT_FOUND, error).into_response();
    }
    match app.store.save_profile(&profile) {
        Ok(()) => Json(profile).into_response(),
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
    // Профиль читается там же и по той же причине: его могли переключить или
    // поправить между ходами, и запрос должен уйти с тем, что выбрано сейчас.
    let profile = app.store.active_profile_or_empty();

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
            match app.agent.ask(&mut chat, &profile, &long_term, &working, &request.text, &tx).await
            {
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

        // Память обновляется после ответа, а не до: так в неё попадают и
        // решения самой модели, а человек не ждёт лишнего запроса перед
        // стримом. Один ход — одно обновление на оба слоя; осечка памяти уже
        // отданный ответ не отменяет. Служебный вызов привязан к рабочей
        // памяти: выключили её — платить за раскладку по слоям незачем.
        if answered && chat.settings.layers.working {
            match app.agent.update_memory(&chat, &profile, &long_term, &working).await {
                Ok(reply) => {
                    // Слои перечитываются с диска здесь, под замком, а не
                    // берутся из копии, прочитанной перед стримом: пока шёл
                    // ответ, их мог поправить человек или соседний чат.
                    let _guard = app.memory_lock.lock().await;
                    let mut long_term = app.store.long_term();
                    let mut working = app.store.working(&chat.id);
                    // Профиль тоже перечитываем: предложения ложатся в тот,
                    // что выбран сейчас, а его могли переключить, пока шёл
                    // ответ.
                    let mut profile = app.store.active_profile_or_empty();
                    let info =
                        agent::apply_memory(reply, &mut profile, &mut long_term, &mut working);
                    if let Err(error) = app.store.save_working(&chat.id, &working) {
                        eprintln!("рабочая память не сохранена: {error}");
                    }
                    // Профиль трогаем только если в очередь что-то добавилось
                    // и профиль вообще есть на диске.
                    if info.profile_added > 0 && !profile.id.is_empty() {
                        if let Err(error) = app.store.save_profile(&profile) {
                            eprintln!("профиль не сохранён: {error}");
                        }
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
            Event::MemoryError(error) => ("memory_error", json!({ "error": error })),
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
