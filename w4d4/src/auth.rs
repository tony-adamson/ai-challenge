//! Вход по одноразовому коду TOTP: один пользователь, один секрет из
//! `TOTP_SECRET`. Сессии — случайные токены в cookie, список лежит в
//! `data/sessions.json` и переживает перезапуск.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;
use totp_rs::{Builder, Secret, Totp};

pub const SESSIONS_FILE: &str = "data/sessions.json";
const COOKIE: &str = "session";
const MAX_AGE: u64 = 30 * 24 * 3600;
const MAX_FAILURES: usize = 5;
const FAILURE_WINDOW: u64 = 60;
/// Второй лимит: перебор по 5 кодов в минуту за час дал бы 300 попыток.
const MAX_FAILURES_HOUR: usize = 20;
const FAILURE_WINDOW_HOUR: u64 = 3600;
/// Без сессии доступны только страница входа и то, что ей нужно для вида.
const PUBLIC: [&str; 3] = ["/login", "/static/colors_and_type.css", "/static/fonts/Manrope-VariableFont_wght.ttf"];

fn totp(secret: Vec<u8>) -> Result<Totp, String> {
    // По умолчанию SHA1, 6 цифр, шаг 30 с, допуск ±1 шаг — как у приложений-аутентификаторов.
    Builder::new().with_secret(secret).with_issuer(Some("Исследователь")).with_account_name("anton")
        .build().map_err(|e| format!("TOTP: {e}"))
}

/// `--totp-init`: новый секрет и otpauth-ссылка для приложения-аутентификатора.
pub fn init() -> Result<String, String> {
    let mut bytes = [0u8; 20];
    getrandom::fill(&mut bytes).map_err(|e| format!("нет источника случайности: {e}"))?;
    let secret = Secret::from(bytes);
    let url = totp(secret.as_bytes().to_vec())?.to_url().map_err(|e| format!("TOTP: {e}"))?;
    Ok(format!("TOTP_SECRET={}\n\nДобавь строку выше в корневой .env и отсканируй или вставь в приложение-аутентификатор:\n{url}",
        secret.to_base32()))
}

fn system_time() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub struct Auth {
    totp: Totp,
    path: String,
    /// токен → время создания, секунды Unix.
    sessions: Mutex<HashMap<String, u64>>,
    /// Время неверных кодов, секунды Unix, за последний час.
    failures: Mutex<VecDeque<u64>>,
    /// Последний принятый шаг TOTP: один код не пускает дважды.
    last_step: Mutex<u64>,
    clock: fn() -> u64,
}

impl Auth {
    pub fn new(secret_base32: &str, path: &str) -> Result<Auth, String> {
        Self::with_clock(secret_base32, path, system_time)
    }

    fn with_clock(secret_base32: &str, path: &str, clock: fn() -> u64) -> Result<Auth, String> {
        let secret = Secret::try_from_base32(secret_base32.trim())
            .map_err(|_| "TOTP_SECRET не похож на base32: сгенерируй его через --totp-init")?;
        let sessions: HashMap<String, u64> = std::fs::read_to_string(path).ok()
            .and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Ok(Auth { totp: totp(secret.as_bytes().to_vec())?, path: path.into(), sessions: Mutex::new(sessions),
            failures: Mutex::new(VecDeque::new()), last_step: Mutex::new(0), clock })
    }

    fn valid(&self, token: &str) -> bool {
        let now = (self.clock)();
        self.sessions.lock().unwrap().get(token).is_some_and(|created| created + MAX_AGE > now)
    }

    fn save(&self, sessions: &HashMap<String, u64>) -> Result<(), String> {
        let text = serde_json::to_string(sessions).expect("сессии сериализуются");
        std::fs::write(&self.path, text).map_err(|e| format!("сессии не сохранены: {e}"))
    }

    fn start(&self) -> Result<String, String> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| format!("нет источника случайности: {e}"))?;
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let now = (self.clock)();
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, created| *created + MAX_AGE > now);
        sessions.insert(token.clone(), now);
        self.save(&sessions)?;
        Ok(token)
    }

    fn end(&self, token: &str) {
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.remove(token).is_some() {
            if let Err(error) = self.save(&sessions) {
                eprintln!("{error}");
            }
        }
    }

    fn check(&self, code: &str) -> bool {
        let Some(step) = self.totp.check(code.trim(), (self.clock)()) else { return false };
        let mut last = self.last_step.lock().unwrap();
        if step <= *last {
            return false;
        }
        *last = step;
        true
    }

    /// Пять неверных кодов за минуту или двадцать за час — дальше 429 до конца окна.
    fn limited(&self) -> bool {
        let now = (self.clock)();
        let mut failures = self.failures.lock().unwrap();
        while failures.front().is_some_and(|at| now.saturating_sub(*at) > FAILURE_WINDOW_HOUR) {
            failures.pop_front();
        }
        let last_minute = failures.iter().filter(|at| now.saturating_sub(**at) <= FAILURE_WINDOW).count();
        last_minute >= MAX_FAILURES || failures.len() >= MAX_FAILURES_HOUR
    }

    fn fail(&self) {
        self.failures.lock().unwrap().push_back((self.clock)());
    }
}

fn cookie(request: &Request) -> Option<String> {
    request.headers().get_all(header::COOKIE).iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE)
        .map(|(_, value)| value.to_string())
}

fn page(status: StatusCode, error: &str) -> Response {
    let html = include_str!("../static/login.html").replace("{{error}}", error);
    (status, Html(html)).into_response()
}

#[derive(Deserialize)]
struct LoginForm {
    code: String,
}

async fn login_page() -> Response {
    page(StatusCode::OK, "")
}

async fn login(State(auth): State<Arc<Auth>>, Form(form): Form<LoginForm>) -> Response {
    if auth.limited() {
        return page(StatusCode::TOO_MANY_REQUESTS, "Слишком много неверных кодов. Подожди минуту, а после 20 ошибок за час — час.");
    }
    if !auth.check(&form.code) {
        auth.fail();
        return page(StatusCode::UNAUTHORIZED, "Код не подошёл. Введи текущий код из приложения.");
    }
    match auth.start() {
        Ok(token) => {
            let cookie = format!("{COOKIE}={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={MAX_AGE}");
            ([(header::SET_COOKIE, cookie)], Redirect::to("/")).into_response()
        }
        Err(error) => page(StatusCode::INTERNAL_SERVER_ERROR, &error),
    }
}

async fn logout(State(auth): State<Arc<Auth>>, request: Request) -> Response {
    if let Some(token) = cookie(&request) {
        auth.end(&token);
    }
    let clear = format!("{COOKIE}=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0");
    ([(header::SET_COOKIE, clear)], Redirect::to("/login")).into_response()
}

pub fn routes(auth: Arc<Auth>) -> Router {
    Router::new()
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .with_state(auth)
}

/// Всё, кроме входа, — только с живой сессией: API отвечает 401, страницы
/// уводят на /login.
pub async fn require(State(auth): State<Arc<Auth>>, request: Request, next: Next) -> Response {
    if PUBLIC.contains(&request.uri().path()) || cookie(&request).is_some_and(|t| auth.valid(&t)) {
        return next.run(request).await;
    }
    if request.uri().path().starts_with("/api/") {
        return (StatusCode::UNAUTHORIZED, "нужен вход").into_response();
    }
    Redirect::to("/login").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::redirect::Policy;

    /// Секрет и коды — тестовые векторы RFC 4226/6238 (ASCII "12345678901234567890"):
    /// T=59 → шаг 1 → 287082; соседние шаги 0 → 755224 и 2 → 359152, шаг 3 → 969429.
    const RFC_SECRET: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

    fn rfc_clock() -> u64 {
        59
    }

    async fn serve(auth: Arc<Auth>) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route("/", get(|| async { "чат" }))
            .route("/api/state", get(|| async { "{}" }))
            .merge(routes(auth.clone()))
            .layer(axum::middleware::from_fn_with_state(auth, require));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        (base, tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }))
    }

    fn temp(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("w4d4-auth-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("sessions.json").to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn closed_without_session_and_open_after_code() {
        let path = temp("login");
        let _ = std::fs::remove_file(&path);
        let auth = Arc::new(Auth::with_clock(RFC_SECRET, &path, rfc_clock).unwrap());
        let (base, server) = serve(auth).await;
        let http = reqwest::Client::builder().redirect(Policy::none()).build().unwrap();

        assert_eq!(http.get(format!("{base}/api/state")).send().await.unwrap().status(), 401);
        let page = http.get(format!("{base}/")).send().await.unwrap();
        assert_eq!(page.status(), 303);
        assert_eq!(page.headers()["location"], "/login");
        assert_eq!(http.get(format!("{base}/login")).send().await.unwrap().status(), 200);
        // Код соседнего с допуском шага (шаг 3) не принимается.
        let wrong = http.post(format!("{base}/login")).form(&[("code", "969429")]).send().await.unwrap();
        assert_eq!(wrong.status(), 401);

        let ok = http.post(format!("{base}/login")).form(&[("code", "287082")]).send().await.unwrap();
        assert_eq!(ok.status(), 303);
        let set = ok.headers()["set-cookie"].to_str().unwrap().to_string();
        for part in ["HttpOnly", "Secure", "SameSite=Strict", "Path=/", "Max-Age=2592000"] {
            assert!(set.contains(part), "{part} в {set}");
        }
        let session = set.split(';').next().unwrap().to_string();
        let api = http.get(format!("{base}/api/state")).header("cookie", &session).send().await.unwrap();
        assert_eq!(api.status(), 200);
        // Повтор уже принятого кода и кода предыдущего шага — не вход.
        for code in ["287082", "755224"] {
            let again = http.post(format!("{base}/login")).form(&[("code", code)]).send().await.unwrap();
            assert_eq!(again.status(), 401, "{code}");
        }
        // Следующий шаг в пределах допуска ±1 принимается.
        let skew = http.post(format!("{base}/login")).form(&[("code", "359152")]).send().await.unwrap();
        assert_eq!(skew.status(), 303);
        server.abort();

        // Сессия пережила перезапуск: новый Auth читает тот же файл.
        let restarted = Arc::new(Auth::with_clock(RFC_SECRET, &path, rfc_clock).unwrap());
        let (base, server) = serve(restarted).await;
        let api = http.get(format!("{base}/api/state")).header("cookie", &session).send().await.unwrap();
        assert_eq!(api.status(), 200);
        let out = http.post(format!("{base}/logout")).header("cookie", &session).send().await.unwrap();
        assert_eq!(out.status(), 303);
        assert!(out.headers()["set-cookie"].to_str().unwrap().contains("Max-Age=0"));
        let api = http.get(format!("{base}/api/state")).header("cookie", &session).send().await.unwrap();
        assert_eq!(api.status(), 401);
        server.abort();
    }

    #[tokio::test]
    async fn five_wrong_codes_a_minute_lock_the_login() {
        let auth = Arc::new(Auth::with_clock(RFC_SECRET, &temp("limit"), rfc_clock).unwrap());
        let (base, server) = serve(auth).await;
        let http = reqwest::Client::builder().redirect(Policy::none()).build().unwrap();
        for _ in 0..5 {
            let wrong = http.post(format!("{base}/login")).form(&[("code", "000000")]).send().await.unwrap();
            assert_eq!(wrong.status(), 401);
        }
        // Даже верный код не проходит, пока окно не истекло.
        let locked = http.post(format!("{base}/login")).form(&[("code", "287082")]).send().await.unwrap();
        assert_eq!(locked.status(), 429);
        server.abort();
    }

    static NOW: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn moving_clock() -> u64 {
        NOW.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[tokio::test]
    async fn twenty_wrong_codes_an_hour_lock_the_login() {
        use std::sync::atomic::Ordering::SeqCst;
        let start = 1_000_000;
        NOW.store(start, SeqCst);
        let auth = Arc::new(Auth::with_clock(RFC_SECRET, &temp("hour"), moving_clock).unwrap());
        let code_at = |time: u64| auth.totp.generate(time).to_string();
        let (base, server) = serve(auth.clone()).await;
        let http = reqwest::Client::builder().redirect(Policy::none()).build().unwrap();
        // По одной ошибке в 61 с: минутный лимит не срабатывает, 20 ошибок укладываются в 1220 с.
        for i in 0..20 {
            NOW.store(start + i * 61, SeqCst);
            let wrong = http.post(format!("{base}/login")).form(&[("code", "000000")]).send().await.unwrap();
            assert_eq!(wrong.status(), 401, "попытка {}", i + 1);
        }
        let later = start + 20 * 61;
        NOW.store(later, SeqCst);
        let locked = http.post(format!("{base}/login")).form(&[("code", code_at(later))]).send().await.unwrap();
        assert_eq!(locked.status(), 429);
        // Через час после последней ошибки окно пустое — верный код пускает.
        let free = start + 19 * 61 + 3601;
        NOW.store(free, SeqCst);
        let ok = http.post(format!("{base}/login")).form(&[("code", code_at(free))]).send().await.unwrap();
        assert_eq!(ok.status(), 303);
        server.abort();
    }

    #[test]
    fn init_prints_a_secret_that_auth_accepts() {
        let text = init().unwrap();
        let secret = text.lines().next().unwrap().strip_prefix("TOTP_SECRET=").unwrap();
        assert_eq!(secret.len(), 32);
        assert!(text.contains("otpauth://totp/"));
        assert!(Auth::new(secret, &temp("init")).is_ok());
        assert!(Auth::new("не base32!", &temp("init")).is_err());
    }

    /// Скачивание отчёта: без сессии 401; с сессией 200 с телом и обоими
    /// заголовками; кривое имя и выход за папку не читают ничего лишнего.
    #[tokio::test]
    async fn report_download_needs_session_and_validates_name() {
        let dir = std::env::temp_dir().join(format!("w4d4-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ok.md"), "# Привет").unwrap();
        let path = temp("files");
        let _ = std::fs::remove_file(&path);
        let auth = Arc::new(Auth::with_clock(RFC_SECRET, &path, rfc_clock).unwrap());
        let files = dir.clone();
        let app = Router::new()
            .route("/api/files/{name}", get(move |axum::extract::Path(name): axum::extract::Path<String>| {
                let files = files.clone();
                async move { crate::report_response(&files, &name) }
            }))
            .merge(routes(auth.clone()))
            .layer(axum::middleware::from_fn_with_state(auth, require));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::builder().redirect(Policy::none()).build().unwrap();

        assert_eq!(http.get(format!("{base}/api/files/ok.md")).send().await.unwrap().status(), 401);
        let ok = http.post(format!("{base}/login")).form(&[("code", "287082")]).send().await.unwrap();
        assert_eq!(ok.status(), 303);
        let session = ok.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string();

        let file = http.get(format!("{base}/api/files/ok.md")).header("cookie", &session).send().await.unwrap();
        assert_eq!(file.status(), 200);
        assert_eq!(file.headers()["content-type"], "text/markdown; charset=utf-8");
        assert_eq!(file.headers()["content-disposition"], "attachment; filename=\"ok.md\"");
        assert_eq!(file.bytes().await.unwrap().as_ref(), "# Привет".as_bytes());
        // Имя без `.md` не совпадает с `report_name` побайтово — тоже 400.
        for bad in ["x.txt", "ok"] {
            let denied = http.get(format!("{base}/api/files/{bad}")).header("cookie", &session)
                .send().await.unwrap();
            assert_eq!(denied.status(), 400, "{bad}");
        }
        // Выход за папку: 400 от проверки имени или 404 роутера — без чтения вне папки.
        let escape = http.get(format!("{base}/api/files/..%2Fsecret")).header("cookie", &session)
            .send().await.unwrap();
        assert!(matches!(escape.status().as_u16(), 400 | 404), "{}", escape.status());
        let missing = http.get(format!("{base}/api/files/missing.md")).header("cookie", &session)
            .send().await.unwrap();
        assert_eq!(missing.status(), 404);
        server.abort();
    }
}
