//! Эми (w7d1–w7d2): локальный ассистент разработчика. Окно — вкладка браузера на 127.0.0.1:8765 с токеном,
//! слева карточка проекта, справа чат с `/help` по докам и git.
//! w7d2: `emi review --repo <path> --base <ref> [--head <ref>]` — ревью PR в stdout (для GitHub Actions).
//! w7d3: `emi support [--data <dir>]` — окно поддержки пользователей VPN (FAQ через RAG, CRM через MCP).
//! w7d4: там же вкладка оператора — Эми по цели сама читает тикеты и FAQ через MCP-файлы и готовит правки FAQ.
//! w7d5: `emi support` — публичная заглушка кабинета VPN «Ветрило» (5 витрин) с виджетом Эми вместо mailto:
//! клиента Эми узнаёт по токену кабинета через `CUSTOMER_API`; окно д33/д34 — операторская `/operator?t=`.
//! Адрес — `EMI_ADDR` (по умолчанию 127.0.0.1:8765), все URL в страницах относительные — сайт живёт под префиксом.
mod crm_mcp;
mod customer_api;
mod files_mcp;
mod git_mcp;
mod help;
mod llm;
mod rag;
mod review;
mod support;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Map, Value};

/// Адрес из env: на сервере Эми слушает свой порт за Caddy, локально — привычный 8765.
pub fn addr() -> String {
    std::env::var("EMI_ADDR").ok().filter(|a| !a.trim().is_empty()).unwrap_or_else(|| "127.0.0.1:8765".into())
}
const COMMANDS: &str = "Команды:\n/help <вопрос> — ответ по документации проекта и git, с цитатами\n/help — этот список\n/reindex — пересобрать индекс документации";

struct App {
    token: String,
    repo: PathBuf,
    project: String,
    rag: Arc<Mutex<rag::Rag>>,
    llm: llm::Llm,
    git: git_mcp::Client,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("ошибка: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // .env — общий на весь репозиторий челленджа, рядом с папкой дня.
    let _ = dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../.env"));
    // Поддержке git-репозиторий не нужен: её данные — папка `data` (по умолчанию от текущей папки, запуск из w7d3).
    let data = || flag(&args, "--data").map(|d| PathBuf::from(d.unwrap_or_else(|| "data".into())));
    match args.first().map(String::as_str) {
        Some("mcp-crm") => return crm_mcp::serve(data()?).await,
        Some("mcp-files") => return files_mcp::serve(data()?).await,
        Some("support") => return support::run(llm::Llm::from_env()?, data()?).await,
        _ => {}
    }
    let repo_arg = match flag(&args, "--repo")? {
        Some(r) => PathBuf::from(r),
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    let repo = git_line(&repo_arg, &["rev-parse", "--show-toplevel"]).map(PathBuf::from).map_err(|e| format!("{} — не git-репозиторий: {e}", repo_arg.display()))?;
    if args.first().map(String::as_str) == Some("mcp-git") {
        return git_mcp::serve(repo).await;
    }
    // В CI .env нет: ключи приходят из переменных окружения (секреты GitHub), dotenvy их не перетирает.
    let llm = llm::Llm::from_env()?;
    let project = repo.file_name().map_or("проект".into(), |n| n.to_string_lossy().to_string());
    if args.first().map(String::as_str) == Some("review") {
        let base = flag(&args, "--base")?.ok_or("использование: emi review --repo <path> --base <ref> [--head <ref>]")?;
        let head = flag(&args, "--head")?.unwrap_or_else(|| "HEAD".into());
        print!("{}", review::run(&llm, &repo, &project, &base, &head).await?);
        return Ok(());
    }
    // Bind до тяжёлой загрузки индекса: второй экземпляр должен упасть сразу и понятно.
    let addr = addr();
    let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| format!("порт {addr} занят ({e}) — Эми уже запущена? Закрой её и повтори"))?;

    // Бриф (specs/emi-week7.md): данные проекта — в <git-common-dir>/emi/, общие для всех worktree.
    let common = git_line(&repo, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    let db = PathBuf::from(common).join("emi/index.db");
    let (r, d) = (repo.clone(), db.clone());
    let rag = tokio::task::spawn_blocking(move || rag::Rag::open(&r, &d)).await.map_err(|e| e.to_string())??;
    let git = git_mcp::connect(&repo).await?;
    let token = random_token()?;
    let app = Arc::new(App { token: token.clone(), repo, project, rag: Arc::new(Mutex::new(rag)), llm, git });

    eprintln!("проект: {} · индекс: {} · LLM: {}", app.repo.display(), db.display(), app.llm.names());
    println!("http://{addr}/?t={token}");
    axum::serve(listener, router(app)).await.map_err(|e| format!("сервер остановился: {e}"))
}

/// Значение флага `--name <value>`; флаг без значения — ошибка, а не тихий дефолт.
fn flag(args: &[String], name: &str) -> Result<Option<String>, String> {
    match args.iter().position(|a| a == name) {
        Some(i) => args.get(i + 1).cloned().map(Some).ok_or(format!("после {name} нужно значение")),
        None => Ok(None),
    }
}

fn git_line(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git").args(["--no-optional-locks", "-C"]).arg(dir).args(args).stdin(Stdio::null()).output().map_err(|e| format!("git не запустился: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Новый токен на каждый запуск: ссылка из прошлой сессии не открывает окно.
fn random_token() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).map_err(|e| format!("нет /dev/urandom для токена: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn router(app: Arc<App>) -> Router {
    let token = app.token.clone();
    let routes = Router::new()
        .route("/", get(page))
        .route("/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../static/app.js")) }))
        .route("/style.css", get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], include_str!("../static/style.css")) }))
        .route("/api/project", get(project))
        .route("/api/chat", post(chat))
        .with_state(app);
    protect(routes, token)
}

/// Все роуты, включая статику, — только с токеном из `?t=` или заголовка `x-emi-token`.
/// Иначе любая страница в браузере могла бы дёргать 127.0.0.1:8765 и читать проект.
fn protect(routes: Router, token: String) -> Router {
    let token = Arc::new(token);
    routes.layer(middleware::from_fn(move |req: Request, next: Next| {
        let token = token.clone();
        async move {
            let from_query = req.uri().query().unwrap_or("").split('&').find_map(|kv| kv.strip_prefix("t=")).map(str::to_string);
            let from_header = req.headers().get("x-emi-token").and_then(|v| v.to_str().ok()).map(str::to_string);
            if from_query.as_deref() == Some(token.as_str()) || from_header.as_deref() == Some(token.as_str()) {
                next.run(req).await
            } else {
                (StatusCode::FORBIDDEN, "нужен токен из ссылки, которую напечатала Эми при запуске").into_response()
            }
        }
    }))
}

async fn page(State(app): State<Arc<App>>) -> Html<String> {
    Html(include_str!("../static/index.html").replace("{{TOKEN}}", &app.token).replace("{{PROJECT}}", &escape(&app.project)))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

async fn project(State(app): State<Arc<App>>) -> Response {
    let mut card = json!({"name": app.project, "path": app.repo.display().to_string()});
    for (key, tool, args) in [("branch", "git_branch", json!({})), ("status", "git_status", json!({})), ("log", "git_log", json!({"limit": 8}))] {
        let args: Map<String, Value> = args.as_object().cloned().unwrap_or_default();
        card[key] = match git_mcp::call(&app.git, tool, args).await {
            Ok(text) => json!(text),
            Err(e) => json!(format!("ошибка: {e}")),
        };
    }
    Json(card).into_response()
}

async fn chat(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let text = body["text"].as_str().unwrap_or("").trim().to_string();
    let reply = match text.split_once(char::is_whitespace).map_or((text.as_str(), ""), |(c, rest)| (c, rest.trim())) {
        ("/help", "") => json!({"kind": "text", "text": COMMANDS}),
        ("/help", question) => help::answer(&app.llm, app.rag.clone(), &app.git, &app.project, question)
            .await
            .unwrap_or_else(|e| json!({"kind": "error", "text": e})),
        ("/reindex", _) => {
            let rag = app.rag.clone();
            let done = tokio::task::spawn_blocking(move || rag.lock().map_err(|_| "индекс повреждён паникой — перезапусти Эми".to_string())?.reindex()).await;
            match done {
                Ok(Ok(text)) => json!({"kind": "text", "text": text}),
                Ok(Err(e)) => json!({"kind": "error", "text": e}),
                Err(e) => json!({"kind": "error", "text": e.to_string()}),
            }
        }
        ("", _) => json!({"kind": "text", "text": COMMANDS}),
        _ => json!({"kind": "text", "text": format!("Пока я отвечаю только на команды.\n{COMMANDS}")}),
    };
    Json(reply).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn requests_without_valid_token_get_403() {
        let routes = Router::new().route("/", get(|| async { "ok" })).route("/api/project", get(|| async { "card" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, protect(routes, "s3cret".into())).await.unwrap() });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let status = |r: reqwest::Response| r.status().as_u16();
        assert_eq!(status(http.get(format!("{base}/")).send().await.unwrap()), 403);
        assert_eq!(status(http.get(format!("{base}/api/project")).send().await.unwrap()), 403);
        assert_eq!(status(http.get(format!("{base}/?t=wrong")).send().await.unwrap()), 403);
        assert_eq!(status(http.get(format!("{base}/api/project")).header("x-emi-token", "wrong").send().await.unwrap()), 403);
        assert_eq!(status(http.get(format!("{base}/?t=s3cret")).send().await.unwrap()), 200);
        assert_eq!(status(http.get(format!("{base}/api/project")).header("x-emi-token", "s3cret").send().await.unwrap()), 200);
        server.abort();
    }
}
