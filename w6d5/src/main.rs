// Шлюз к локальной модели: Bearer-токен, лимиты и очередь перед mlx_lm.server.
// mlx_lm.server генерирует по одному запросу и ограничений не знает — всё, что видно из сети, проходит здесь.
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::Semaphore;

const MAX_WAITING: usize = 3; // сверх одной генерации
const RATE_PER_MIN: usize = 6; // запросов к модели в минуту с одного IP
const MAX_CONTEXT_CHARS: usize = 16_000; // суммарно по messages
const MAX_TOKENS: u64 = 512;
const RATE_WINDOW: Duration = Duration::from_secs(60);

struct App {
    token: String,
    mlx: String,
    client: reqwest::Client,
    generation: Arc<Semaphore>,  // один permit: mlx_lm.server всё равно генерирует по одному
    admitted: AtomicUsize,  // генерация + ожидающие
    hits: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl App {
    fn new(token: String, mlx: String) -> Self {
        App {
            token,
            mlx,
            client: reqwest::Client::builder().no_proxy().build().expect("reqwest client"),
            generation: Arc::new(Semaphore::new(1)),
            admitted: AtomicUsize::new(0),
            hits: Mutex::new(HashMap::new()),
        }
    }
}

// Место в очереди освобождается и при обрыве клиента: axum роняет future обработчика, guard уходит в drop.
struct Admitted(Arc<App>);
impl Drop for Admitted {
    fn drop(&mut self) {
        self.0.admitted.fetch_sub(1, Ordering::SeqCst);
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": { "message": message, "code": status.as_u16() } }))).into_response()
}

fn with_retry_after(mut response: Response, seconds: u64) -> Response {
    response.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(seconds.max(1)));
    response
}

fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(|| async { Html(include_str!("../chat.html")) }))
        .route("/health", get(health))
        .route("/v1/chat/completions", post(chat))
        .with_state(app)
}

// Caddy ставит X-Forwarded-For; через туннель адрес сокета у всех 127.0.0.1.
fn client_ip(headers: &HeaderMap, addr: SocketAddr) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|ip| ip.trim().to_string())
        .filter(|ip| !ip.is_empty())
        .unwrap_or_else(|| addr.ip().to_string())
}

// Скользящее окно: None — можно, Some(секунд) — сколько ждать.
fn rate_check(app: &App, ip: &str) -> Option<u64> {
    let now = Instant::now();
    let mut hits = app.hits.lock().unwrap();
    hits.retain(|_, q| q.back().is_some_and(|t| now.duration_since(*t) < RATE_WINDOW));
    let queue = hits.entry(ip.to_string()).or_default();
    while queue.front().is_some_and(|t| now.duration_since(*t) >= RATE_WINDOW) {
        queue.pop_front();
    }
    if queue.len() >= RATE_PER_MIN {
        let oldest = *queue.front().unwrap();
        return Some((RATE_WINDOW - now.duration_since(oldest)).as_secs() + 1);
    }
    queue.push_back(now);
    None
}

fn context_chars(body: &Value) -> usize {
    body["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .map(|m| match &m["content"] {
                    Value::String(s) => s.chars().count(),
                    other => other.to_string().chars().count(),
                })
                .sum()
        })
        .unwrap_or(0)
}

async fn chat(
    State(app): State<Arc<App>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    raw: Bytes,
) -> Response {
    // Лимит до проверки токена: перебор токена упирается в те же 6 запросов в минуту.
    let ip = client_ip(&headers, addr);
    if let Some(wait) = rate_check(&app, &ip) {
        let message = format!("Не больше {RATE_PER_MIN} запросов в минуту. Повтори через {wait} с.");
        return with_retry_after(error(StatusCode::TOO_MANY_REQUESTS, &message), wait);
    }
    let bearer = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    if bearer != Some(format!("Bearer {}", app.token).as_str()) {
        return error(StatusCode::UNAUTHORIZED, "Нужен токен: заголовок Authorization: Bearer <LLM_TOKEN>.");
    }
    let mut body: Value = match serde_json::from_slice(&raw) {
        Ok(v @ Value::Object(_)) if v["messages"].is_array() => v,
        _ => return error(StatusCode::BAD_REQUEST, "Ожидается JSON с полем messages (формат OpenAI Chat Completions)."),
    };
    // Стриминг не поддерживаем: очередь и лимиты проще держать на целом ответе.
    if body["stream"] == json!(true) {
        return error(StatusCode::BAD_REQUEST, "stream: true не поддерживается — шлюз отдаёт ответ целиком.");
    }
    let chars = context_chars(&body);
    if chars > MAX_CONTEXT_CHARS {
        let message = format!("Слишком длинный контекст: {chars} символов, лимит {MAX_CONTEXT_CHARS}. Начни новый разговор.");
        return error(StatusCode::PAYLOAD_TOO_LARGE, &message);
    }

    let fits = app
        .admitted
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < 1 + MAX_WAITING).then_some(n + 1));
    if fits.is_err() {
        let message = format!("Модель занята: идёт генерация и {MAX_WAITING} запроса ждут. Повтори позже.");
        return with_retry_after(error(StatusCode::SERVICE_UNAVAILABLE, &message), 30);
    }
    let admitted = Admitted(app.clone());
    let permit = app.generation.clone().acquire_owned().await.expect("semaphore is never closed");

    let max_tokens = body["max_tokens"].as_u64().unwrap_or(MAX_TOKENS).clamp(1, MAX_TOKENS);
    let fields = body.as_object_mut().expect("checked above");
    // Только известные поля: mlx_lm.server берёт из запроса и model/adapters/draft_model — пути к весам,
    // которые он грузит с диска или скачивает. Без model берёт загруженную модель (default_model).
    fields.retain(|key, _| ["messages", "temperature", "top_p", "top_k", "stop", "seed"].contains(&key.as_str()));
    fields.insert("max_tokens".into(), json!(max_tokens));
    fields.insert("chat_template_kwargs".into(), json!({ "enable_thinking": false }));
    // Рекомендованный сэмплинг Qwen3.5 без размышлений, если клиент не задал свой.
    fields.entry("temperature").or_insert(json!(0.7));
    fields.entry("top_p").or_insert(json!(0.8));
    fields.entry("top_k").or_insert(json!(20));

    // mlx без стрима пишет в сокет только в конце и обрыв клиента не замечает — генерация идёт дальше.
    // Поэтому место в очереди и permit держит отдельная задача до ответа mlx, а не обработчик клиента.
    let task = tokio::spawn(async move {
        let _held = (admitted, permit);
        proxy(&app, body).await
    });
    task.await.unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Шлюз упал на запросе."))
}

async fn proxy(app: &App, body: Value) -> Response {
    let sent = app
        .client
        .post(format!("{}/v1/chat/completions", app.mlx))
        .json(&body)
        .timeout(Duration::from_secs(300))
        .send()
        .await;
    let upstream = match sent {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mlx недоступен: {e}");
            return error(StatusCode::BAD_GATEWAY, "Модель недоступна: mlx_lm.server не отвечает.");
        }
    };
    let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    match upstream.bytes().await {
        Ok(bytes) => (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
        Err(e) => {
            eprintln!("обрыв ответа mlx: {e}");
            error(StatusCode::BAD_GATEWAY, "Модель оборвала ответ.")
        }
    }
}

async fn health(State(app): State<Arc<App>>) -> Json<Value> {
    let mlx = app
        .client
        .get(format!("{}/health", app.mlx))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success());
    let busy = app.generation.available_permits() == 0;
    let admitted = app.admitted.load(Ordering::SeqCst);
    Json(json!({
        "mlx": mlx,
        "busy": busy,
        "queued": admitted.saturating_sub(busy as usize),
        "limits": {
            "concurrent": 1,
            "queue": MAX_WAITING,
            "per_minute": RATE_PER_MIN,
            "context_chars": MAX_CONTEXT_CHARS,
            "max_tokens": MAX_TOKENS,
        },
    }))
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("Ошибка: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let _ = dotenvy::dotenv(); // корневой .env ищется вверх по каталогам
    let token = std::env::var("LLM_TOKEN").unwrap_or_default();
    if token.trim().is_empty() {
        return Err("LLM_TOKEN не задан: добавь строку LLM_TOKEN=<секрет> в корневой .env".into());
    }
    let bind = std::env::var("GATEWAY_ADDR").unwrap_or_else(|_| "127.0.0.1:8090".into());
    let mlx = std::env::var("MLX_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| format!("не удалось занять {bind}: {e}"))?;
    println!("Шлюз на http://{bind} → {mlx}");
    let app = Arc::new(App::new(token.trim().to_string(), mlx));
    axum::serve(listener, router(app).into_make_service_with_connect_info::<SocketAddr>())
        .await
        .map_err(|e| format!("сервер упал: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "test-token";

    // Мок mlx: держит ответ delay, считает пик одновременных запросов и возвращает тело, которое получил.
    async fn mock_mlx(delay: Duration) -> (String, Arc<AtomicUsize>) {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (a, p) = (active.clone(), peak.clone());
        let mock = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let (a, p) = (a.clone(), p.clone());
                async move {
                    let now = a.fetch_add(1, Ordering::SeqCst) + 1;
                    p.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(delay).await;
                    a.fetch_sub(1, Ordering::SeqCst);
                    Json(json!({ "choices": [{ "message": { "role": "assistant", "content": "ok" } }], "echo": body }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        (url, peak)
    }

    async fn gateway(mlx: String) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Arc::new(App::new(TOKEN.into(), mlx));
        tokio::spawn(async move {
            axum::serve(listener, router(app).into_make_service_with_connect_info::<SocketAddr>()).await.unwrap()
        });
        url
    }

    async fn ask(url: &str, ip: &str, token: Option<&str>, body: Value) -> reqwest::Response {
        let mut request = reqwest::Client::new()
            .post(format!("{url}/v1/chat/completions"))
            .header("x-forwarded-for", format!("{ip}, 10.0.0.1"))
            .json(&body);
        if let Some(t) = token {
            request = request.bearer_auth(t);
        }
        request.send().await.unwrap()
    }

    fn hello() -> Value {
        json!({ "model": "gpt-4", "adapters": "/tmp/x", "draft_model": "x/y", "max_tokens": 4096, "messages": [{ "role": "user", "content": "привет" }] })
    }

    #[tokio::test]
    async fn rejects_missing_or_wrong_token() {
        let (mlx, _) = mock_mlx(Duration::ZERO).await;
        let url = gateway(mlx).await;
        assert_eq!(ask(&url, "1.1.1.1", None, hello()).await.status(), 401);
        assert_eq!(ask(&url, "1.1.1.1", Some("wrong"), hello()).await.status(), 401);
        // Перебор токена ограничен тем же лимитом частоты.
        for _ in 0..4 {
            ask(&url, "1.1.1.1", Some("wrong"), hello()).await;
        }
        assert_eq!(ask(&url, "1.1.1.1", Some("wrong"), hello()).await.status(), 429);
    }

    #[tokio::test]
    async fn proxies_answer_with_forced_params() {
        let (mlx, _) = mock_mlx(Duration::ZERO).await;
        let url = gateway(mlx).await;
        let response = ask(&url, "1.1.1.1", Some(TOKEN), hello()).await;
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "ok");
        let sent = &body["echo"];
        assert_eq!(sent["max_tokens"], 512);
        assert_eq!(sent["chat_template_kwargs"]["enable_thinking"], false);
        for field in ["model", "adapters", "draft_model"] {
            assert!(sent.get(field).is_none(), "{field} ушло в mlx");
        }
    }

    #[tokio::test]
    async fn long_context_is_413() {
        let (mlx, _) = mock_mlx(Duration::ZERO).await;
        let url = gateway(mlx).await;
        let body = json!({ "messages": [{ "role": "user", "content": "я".repeat(16_001) }] });
        assert_eq!(ask(&url, "1.1.1.1", Some(TOKEN), body).await.status(), 413);
    }

    #[tokio::test]
    async fn seventh_request_per_minute_is_429_per_ip() {
        let (mlx, _) = mock_mlx(Duration::ZERO).await;
        let url = gateway(mlx).await;
        for _ in 0..6 {
            assert_eq!(ask(&url, "2.2.2.2", Some(TOKEN), hello()).await.status(), 200);
        }
        let limited = ask(&url, "2.2.2.2", Some(TOKEN), hello()).await;
        assert_eq!(limited.status(), 429);
        assert!(limited.headers().contains_key("retry-after"));
        // Другой клиент за тем же прокси лимит не делит.
        assert_eq!(ask(&url, "3.3.3.3", Some(TOKEN), hello()).await.status(), 200);
    }

    #[tokio::test]
    async fn queue_is_sequential_and_overflow_is_503() {
        let (mlx, peak) = mock_mlx(Duration::from_millis(300)).await;
        let url = gateway(mlx).await;
        let started = Instant::now();
        let mut tasks = Vec::new();
        for i in 0..4 {
            let url = url.clone();
            tasks.push(tokio::spawn(async move { ask(&url, &format!("10.0.0.{i}"), Some(TOKEN), hello()).await.status() }));
        }
        tokio::time::sleep(Duration::from_millis(100)).await; // все четыре уже в шлюзе
        assert_eq!(ask(&url, "10.0.0.9", Some(TOKEN), hello()).await.status(), 503);
        for task in tasks {
            assert_eq!(task.await.unwrap(), 200);
        }
        assert_eq!(peak.load(Ordering::SeqCst), 1, "в mlx больше одной генерации разом");
        assert!(started.elapsed() >= Duration::from_millis(1200), "четыре ответа по 300 мс шли параллельно");
    }

    #[tokio::test]
    async fn client_abort_keeps_mlx_busy_until_it_finishes() {
        let (mlx, peak) = mock_mlx(Duration::from_millis(400)).await;
        let url = gateway(mlx).await;
        // Клиент бросает запрос посреди генерации; mlx (как и мок) продолжает её.
        let aborted = reqwest::Client::new()
            .post(format!("{url}/v1/chat/completions"))
            .header("x-forwarded-for", "4.4.4.4")
            .bearer_auth(TOKEN)
            .json(&hello())
            .timeout(Duration::from_millis(100))
            .send()
            .await;
        assert!(aborted.is_err());
        assert_eq!(ask(&url, "5.5.5.5", Some(TOKEN), hello()).await.status(), 200);
        assert_eq!(peak.load(Ordering::SeqCst), 1, "после обрыва клиента в mlx ушла вторая генерация");
    }
}
