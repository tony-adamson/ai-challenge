//! MCP-сервер наблюдений по Streamable HTTP: поиск GitHub, наблюдения за
//! поисковым запросом, планировщик снимков и агрегированная сводка.
//! Данные — SQLite (`data/watch.db`); время хранится строками RFC3339 UTC,
//! их считает сама SQLite, поэтому сравнение строк совпадает со сравнением дат.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::{
    model::{CallToolRequestParams, CallToolResponse, CallToolResult, Implementation,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool},
    service::RequestContext,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ErrorData, RoleServer, ServerHandler,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::github::{self, Github, Search};

pub const DEFAULT_ADDR: &str = "127.0.0.1:8800";
const DEFAULT_DB: &str = "data/watch.db";
/// Как часто планировщик смотрит, у кого подошло время.
const TICK: Duration = Duration::from_secs(20);
const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%SZ','now')";
const TOP: usize = 5;
/// У поиска GitHub без токена 10 запросов в минуту: больше наблюдений не прокормить.
const MAX_WATCHES: i64 = 5;
/// Запуски старше этого срока удаляются вместе со снимками на каждом тике.
const RETENTION_DAYS: u32 = 7;

const SCHEMA: &str = "
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS watches (
    id INTEGER PRIMARY KEY,
    query TEXT NOT NULL,
    every_minutes INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    last_run_at TEXT
);
CREATE TABLE IF NOT EXISTS runs (
    id INTEGER PRIMARY KEY,
    watch_id INTEGER NOT NULL REFERENCES watches(id) ON DELETE CASCADE,
    started_at TEXT NOT NULL,
    status TEXT NOT NULL,
    error TEXT
);
CREATE TABLE IF NOT EXISTS repos_snapshot (
    run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    full_name TEXT NOT NULL,
    url TEXT,
    description TEXT,
    language TEXT,
    stars INTEGER NOT NULL,
    pushed_at TEXT
);
CREATE INDEX IF NOT EXISTS runs_by_watch ON runs(watch_id, started_at);
CREATE INDEX IF NOT EXISTS repos_by_run ON repos_snapshot(run_id);
";

struct Repo {
    name: String,
    url: Option<String>,
    description: Option<String>,
    language: Option<String>,
    stars: i64,
    pushed_at: Option<String>,
}

pub struct Db(Mutex<Connection>);

fn sql(e: rusqlite::Error) -> String {
    format!("SQLite: {e}")
}

impl Db {
    pub fn open(path: &str) -> Result<Db, String> {
        if let Some(dir) = std::path::Path::new(path).parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("не создать {}: {e}", dir.display()))?;
        }
        let conn = Connection::open(path).map_err(sql)?;
        conn.execute_batch(SCHEMA).map_err(sql)?;
        Ok(Db(Mutex::new(conn)))
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn now(&self) -> Result<String, String> {
        self.conn().query_row(&format!("SELECT {NOW}"), [], |r| r.get(0)).map_err(sql)
    }

    pub fn create(&self, query: &str, every_minutes: u32) -> Result<i64, String> {
        let conn = self.conn();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM watches", [], |r| r.get(0)).map_err(sql)?;
        if count >= MAX_WATCHES {
            return Err(format!("Не больше {MAX_WATCHES} наблюдений: лимит поиска GitHub без токена — 10 запросов в минуту. Удалите лишнее через watch_delete."));
        }
        conn.execute(&format!("INSERT INTO watches (query, every_minutes, created_at) VALUES (?1, ?2, {NOW})"),
            params![query.trim(), every_minutes]).map_err(sql)?;
        Ok(conn.last_insert_rowid())
    }

    pub fn list(&self) -> Result<Vec<Value>, String> {
        let conn = self.conn();
        let mut stmt = conn.prepare("
            SELECT w.id, w.query, w.every_minutes, w.created_at, w.last_run_at,
              (SELECT status FROM runs r WHERE r.watch_id = w.id ORDER BY started_at DESC, id DESC LIMIT 1),
              (SELECT error FROM runs r WHERE r.watch_id = w.id ORDER BY started_at DESC, id DESC LIMIT 1),
              (SELECT COUNT(*) FROM runs r WHERE r.watch_id = w.id AND status = 'ok')
            FROM watches w ORDER BY w.id").map_err(sql)?;
        let rows = stmt.query_map([], |r| Ok(json!({
            "id": r.get::<_, i64>(0)?, "query": r.get::<_, String>(1)?,
            "every_minutes": r.get::<_, i64>(2)?, "created_at": r.get::<_, String>(3)?,
            "last_run_at": r.get::<_, Option<String>>(4)?, "last_status": r.get::<_, Option<String>>(5)?,
            "last_error": r.get::<_, Option<String>>(6)?, "snapshots": r.get::<_, i64>(7)?,
        }))).map_err(sql)?;
        rows.collect::<Result<_, _>>().map_err(sql)
    }

    pub fn delete(&self, id: i64) -> Result<bool, String> {
        Ok(self.conn().execute("DELETE FROM watches WHERE id = ?1", [id]).map_err(sql)? > 0)
    }

    /// Наблюдения, у которых подошло время. Просроченное за время простоя
    /// попадает сюда один раз: после запуска `last_run_at` становится «сейчас».
    pub fn due(&self) -> Result<Vec<(i64, String)>, String> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, query FROM watches
            WHERE last_run_at IS NULL OR unixepoch(last_run_at) + every_minutes * 60 <= unixepoch()
            ORDER BY id").map_err(sql)?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(sql)?;
        rows.collect::<Result<_, _>>().map_err(sql)
    }

    /// Запуск наблюдения: успешный — со снимком репозиториев, неудачный — с текстом ошибки.
    pub fn record(&self, watch_id: i64, at: &str, result: &Result<Value, String>) -> Result<(), String> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(sql)?;
        // Наблюдение удалили, пока шёл запрос к GitHub, — записывать некуда.
        if tx.execute("UPDATE watches SET last_run_at = ?2 WHERE id = ?1", params![watch_id, at]).map_err(sql)? == 0 {
            return Ok(());
        }
        let (status, error) = match result {
            Ok(_) => ("ok", None),
            Err(error) => ("error", Some(error.as_str())),
        };
        tx.execute("INSERT INTO runs (watch_id, started_at, status, error) VALUES (?1, ?2, ?3, ?4)",
            params![watch_id, at, status, error]).map_err(sql)?;
        let run_id = tx.last_insert_rowid();
        if let Ok(value) = result {
            for repo in value["repositories"].as_array().into_iter().flatten() {
                tx.execute("INSERT INTO repos_snapshot (run_id, full_name, url, description, language, stars, pushed_at)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![run_id, repo["name"].as_str().unwrap_or(""),
                    repo["url"].as_str(), repo["description"].as_str(), repo["language"].as_str(),
                    repo["stars"].as_i64().unwrap_or(0), repo["pushed_at"].as_str()]).map_err(sql)?;
            }
        }
        tx.commit().map_err(sql)
    }

    /// Удаляет запуски старше `RETENTION_DAYS` дней; снимки уходят каскадом
    /// (`ON DELETE CASCADE`, `PRAGMA foreign_keys = ON` в схеме).
    pub fn prune(&self) -> Result<usize, String> {
        self.conn().execute(&format!("DELETE FROM runs WHERE started_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-{RETENTION_DAYS} days')"), [])
            .map_err(sql)
    }

    /// Сводка между `since` и последним успешным снимком. База сравнения —
    /// последний снимок не позже `since`; если его нет — первый снимок вообще.
    pub fn summary(&self, id: i64, since: Option<&str>) -> Result<Value, String> {
        let conn = self.conn();
        let (query, created_at): (String, String) = conn.query_row(
            "SELECT query, created_at FROM watches WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional().map_err(sql)?.ok_or(format!("Наблюдение {id} не найдено"))?;
        let given = since.is_some();
        let since = match since {
            Some(raw) => conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%SZ', ?1)", [raw], |r| r.get::<_, Option<String>>(0))
                .map_err(sql)?.ok_or("since должен быть датой RFC3339, например 2026-09-23T10:00:00Z")?,
            None => created_at,
        };
        // Снимок ровно в момент since — база прошлой сводки, в новый период он не входит.
        let after = if given { ">" } else { ">=" };
        let (runs_ok, runs_error): (i64, i64) = conn.query_row(&format!(
            "SELECT COALESCE(SUM(status = 'ok'), 0), COALESCE(SUM(status = 'error'), 0)
             FROM runs WHERE watch_id = ?1 AND started_at {after} ?2"), params![id, since],
            |r| Ok((r.get(0)?, r.get(1)?))).map_err(sql)?;
        let last_error: Option<String> = conn.query_row(&format!(
            "SELECT error FROM runs WHERE watch_id = ?1 AND started_at {after} ?2 AND status = 'error'
             ORDER BY started_at DESC, id DESC LIMIT 1"), params![id, since], |r| r.get(0))
            .optional().map_err(sql)?;
        let run = |filter: &str, order: &str| conn.query_row(
            &format!("SELECT id, started_at FROM runs WHERE watch_id = ?1 AND status = 'ok' {filter}
                      ORDER BY started_at {order}, id {order} LIMIT 1"),
            params![id, since], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))).optional().map_err(sql);
        let mut summary = json!({"watch_id": id, "query": query, "since": since,
            "runs_ok": runs_ok, "runs_error": runs_error, "last_error": last_error,
            "first_snapshot_at": null, "last_snapshot_at": null,
            "new_repos": [], "star_growth": [], "fresh_pushes": []});
        let Some(latest) = run("AND ?2 = ?2", "DESC")? else {
            summary["note"] = json!("Снимков пока нет");
            return Ok(summary);
        };
        let base = match run("AND started_at <= ?2", "DESC")? {
            Some(base) => base,
            None => run("AND ?2 = ?2", "ASC")?.expect("есть хотя бы последний снимок"),
        };
        let latest_repos = repos(&conn, latest.0)?;
        let base_repos = repos(&conn, base.0)?;
        let stars_before = |name: &str| base_repos.iter().find(|r| r.name == name).map(|r| r.stars);

        let new_repos: Vec<Value> = latest_repos.iter().filter(|r| stars_before(&r.name).is_none())
            .map(|r| json!({"name": r.name, "url": r.url, "description": r.description,
                "language": r.language, "stars": r.stars})).collect();
        let mut growth: Vec<(&Repo, i64)> = latest_repos.iter()
            .filter_map(|r| stars_before(&r.name).map(|before| (r, r.stars - before)))
            .filter(|(_, delta)| *delta > 0).collect();
        growth.sort_by_key(|(_, delta)| std::cmp::Reverse(*delta));
        let mut fresh: Vec<&Repo> = latest_repos.iter()
            .filter(|r| r.pushed_at.as_deref().is_some_and(|at| at > since.as_str())).collect();
        fresh.sort_by(|a, b| b.pushed_at.cmp(&a.pushed_at));

        summary["first_snapshot_at"] = json!(base.1);
        summary["last_snapshot_at"] = json!(latest.1);
        summary["repos_in_last_snapshot"] = json!(latest_repos.len());
        summary["new_repos"] = json!(new_repos);
        summary["star_growth"] = json!(growth.iter().take(TOP).map(|(r, delta)|
            json!({"name": r.name, "url": r.url, "stars": r.stars, "delta": delta})).collect::<Vec<_>>());
        summary["fresh_pushes"] = json!(fresh.iter().take(TOP * 2).map(|r|
            json!({"name": r.name, "url": r.url, "pushed_at": r.pushed_at})).collect::<Vec<_>>());
        Ok(summary)
    }
}

fn repos(conn: &Connection, run_id: i64) -> Result<Vec<Repo>, String> {
    let mut stmt = conn.prepare("SELECT full_name, url, description, language, stars, pushed_at
        FROM repos_snapshot WHERE run_id = ?1").map_err(sql)?;
    let rows = stmt.query_map([run_id], |r| Ok(Repo { name: r.get(0)?, url: r.get(1)?,
        description: r.get(2)?, language: r.get(3)?, stars: r.get(4)?, pushed_at: r.get(5)? })).map_err(sql)?;
    rows.collect::<Result<_, _>>().map_err(sql)
}

/// Один проход планировщика: подошедшие наблюдения по очереди, не параллельно —
/// у поиска GitHub без токена 10 запросов в минуту.
pub async fn run_due(github: &Github, db: &Db) -> Result<usize, String> {
    db.prune()?;
    let due = db.due()?;
    for (id, query) in &due {
        let result = github.snapshot(query).await;
        if let Err(error) = &result {
            eprintln!("наблюдение {id}: {error}");
        }
        db.record(*id, &db.now()?, &result)?;
    }
    Ok(due.len())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    query: String,
    every_minutes: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ById {
    id: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SummaryArgs {
    id: i64,
    #[serde(default)]
    since: Option<String>,
}

fn schema(value: Value) -> Arc<serde_json::Map<String, Value>> {
    Arc::new(value.as_object().expect("literal schema is an object").clone())
}

pub fn tools() -> Vec<Tool> {
    vec![
        github::tool(),
        Tool::new("watch_create",
            "Create a scheduled watch over a GitHub repository search. The server takes a snapshot of the top 30 repositories every every_minutes minutes and stores it. Returns the watch id.",
            schema(json!({"type":"object","properties":{
                "query":{"type":"string","minLength":1,"maxLength":256,
                    "description":"GitHub repository search query, same syntax as search_repositories"},
                "every_minutes":{"type":"integer","minimum":1,"maximum":1440,
                    "description":"Snapshot interval in minutes (1–1440)"}},
                "required":["query","every_minutes"],"additionalProperties":false}))),
        Tool::new("watch_list",
            "List watches: id, query, interval, created_at, last_run_at, last_status and number of stored snapshots.",
            schema(json!({"type":"object","properties":{},"additionalProperties":false}))),
        Tool::new("watch_delete", "Delete a watch and all its snapshots.",
            schema(json!({"type":"object","properties":{"id":{"type":"integer","minimum":1}},
                "required":["id"],"additionalProperties":false}))),
        Tool::new("watch_summary",
            "Aggregate a watch between since and the latest snapshot: new repositories, top star growth, repositories pushed after since, successful and failed runs.",
            schema(json!({"type":"object","properties":{
                "id":{"type":"integer","minimum":1},
                "since":{"type":"string","description":"RFC3339 time, e.g. 2026-09-23T10:00:00Z. Default: watch creation time"}},
                "required":["id"],"additionalProperties":false}))),
    ]
}

fn args<T: serde::de::DeserializeOwned>(request: &CallToolRequestParams, hint: &str) -> Result<T, String> {
    serde_json::from_value(Value::Object(request.arguments.clone().unwrap_or_default()))
        .map_err(|_| hint.to_string())
}

#[derive(Clone)]
pub struct Watcher {
    github: Arc<Github>,
    db: Arc<Db>,
}

impl Watcher {
    pub fn new(github: Arc<Github>, db: Arc<Db>) -> Self {
        Self { github, db }
    }

    async fn call(&self, request: &CallToolRequestParams) -> Result<Value, String> {
        match request.name.as_ref() {
            github::TOOL => self.github.search(Search::parse(Value::Object(
                request.arguments.clone().unwrap_or_default()))?).await,
            "watch_create" => {
                let input: Create = args(request, "Нужны query (строка) и every_minutes (целое 1–1440)")?;
                if input.query.trim().is_empty() || input.query.chars().count() > 256 {
                    return Err("query должен содержать от 1 до 256 символов".into());
                }
                if !(1..=1440).contains(&input.every_minutes) {
                    return Err("every_minutes должен быть от 1 до 1440".into());
                }
                let id = self.db.create(&input.query, input.every_minutes)?;
                Ok(json!({"id": id, "query": input.query.trim(), "every_minutes": input.every_minutes,
                    "note": "Первый снимок будет сделан в течение 30 секунд"}))
            }
            "watch_list" => {
                if request.arguments.as_ref().is_some_and(|a| !a.is_empty()) {
                    return Err("watch_list не принимает параметров".into());
                }
                Ok(json!({"watches": self.db.list()?}))
            }
            "watch_delete" => {
                let input: ById = args(request, "Нужен id (целое число)")?;
                match self.db.delete(input.id)? {
                    true => Ok(json!({"deleted": input.id})),
                    false => Err(format!("Наблюдение {} не найдено", input.id)),
                }
            }
            "watch_summary" => {
                let input: SummaryArgs = args(request, "Нужен id (целое число) и необязательный since (RFC3339)")?;
                self.db.summary(input.id, input.since.as_deref())
            }
            _ => Err(String::new()),
        }
    }
}

impl ServerHandler for Watcher {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("github-watch", "0.1.0"))
    }

    async fn list_tools(&self, _: Option<PaginatedRequestParams>, _: RequestContext<RoleServer>)
        -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    async fn call_tool(&self, request: CallToolRequestParams, _: RequestContext<RoleServer>)
        -> Result<CallToolResponse, ErrorData> {
        if !tools().iter().any(|tool| tool.name == request.name) {
            return Err(ErrorData::invalid_params("Неизвестный инструмент", None));
        }
        Ok(match self.call(&request).await {
            Ok(value) => CallToolResult::structured(value),
            Err(error) => CallToolResult::structured_error(json!({"error": error})),
        }.into())
    }
}

pub fn router(watcher: Watcher) -> axum::Router {
    let service = StreamableHttpService::new(move || Ok(watcher.clone()),
        Arc::new(LocalSessionManager::default()), StreamableHttpServerConfig::default());
    axum::Router::new().nest_service("/mcp", service)
}

pub async fn serve() -> Result<(), String> {
    let _ = dotenvy::dotenv();
    let var = |name: &str, default: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.into());
    let (addr, path) = (var("WATCH_MCP_ADDR", DEFAULT_ADDR), var("WATCH_DB", DEFAULT_DB));
    let db = Arc::new(Db::open(&path)?);
    let github = Arc::new(Github::new()?);
    let (scheduler_github, scheduler_db) = (github.clone(), db.clone());
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(TICK);
        loop {
            tick.tick().await;
            if let Err(error) = run_due(&scheduler_github, &scheduler_db).await {
                eprintln!("планировщик: {error}");
            }
        }
    });
    let listener = tokio::net::TcpListener::bind(&addr).await
        .map_err(|e| format!("не удалось занять {addr}: {e}"))?;
    println!("MCP-сервер наблюдений: http://{addr}/mcp · база {path} · тик {} с", TICK.as_secs());
    axum::serve(listener, router(Watcher::new(github, db))).await.map_err(|e| format!("сервер остановился: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::Query, routing::get, Json, Router};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn repo(name: &str, stars: i64, pushed_at: &str) -> Value {
        json!({"name": name, "url": format!("https://github.com/{name}"), "description": "fixture",
            "language": "Rust", "stars": stars, "pushed_at": pushed_at})
    }

    fn snapshot(repos: Vec<Value>) -> Result<Value, String> {
        Ok(json!({"repositories": repos}))
    }

    /// Три фиксированных запуска: снимок, ошибка, снимок. Ожидания выписаны руками.
    fn fixture_db() -> Db {
        let db = Db::open(":memory:").unwrap();
        let id = db.create("search language:Rust", 60).unwrap();
        db.conn().execute("UPDATE watches SET created_at = '2026-01-01T00:00:00Z'", []).unwrap();
        db.record(id, "2026-01-01T01:00:00Z", &snapshot(vec![
            repo("a/a", 10, "2025-12-01T00:00:00Z"), repo("b/b", 5, "2025-12-02T00:00:00Z")])).unwrap();
        db.record(id, "2026-01-01T02:00:00Z", &Err("GitHub вернул HTTP 502".into())).unwrap();
        db.record(id, "2026-01-01T03:00:00Z", &snapshot(vec![
            repo("a/a", 15, "2026-01-01T02:30:00Z"), repo("b/b", 5, "2025-12-02T00:00:00Z"),
            repo("c/c", 1, "2025-11-01T00:00:00Z")])).unwrap();
        db
    }

    #[test]
    fn summary_aggregates_fixed_snapshots() {
        let db = fixture_db();
        let s = db.summary(1, Some("2026-01-01T01:30:00Z")).unwrap();
        assert_eq!(s["first_snapshot_at"], "2026-01-01T01:00:00Z");
        assert_eq!(s["last_snapshot_at"], "2026-01-01T03:00:00Z");
        assert_eq!(s["new_repos"][0]["name"], "c/c");
        assert_eq!(s["new_repos"].as_array().unwrap().len(), 1);
        assert_eq!(s["star_growth"], json!([{"name":"a/a","url":"https://github.com/a/a","stars":15,"delta":5}]));
        assert_eq!(s["fresh_pushes"], json!([{"name":"a/a","url":"https://github.com/a/a","pushed_at":"2026-01-01T02:30:00Z"}]));
        assert_eq!((s["runs_ok"].as_i64(), s["runs_error"].as_i64()), (Some(1), Some(1)));
        assert_eq!(s["last_error"], "GitHub вернул HTTP 502");

        // Без since — с создания: база — первый снимок, запусков три.
        let all = db.summary(1, None).unwrap();
        assert_eq!(all["since"], "2026-01-01T00:00:00Z");
        assert_eq!(all["new_repos"][0]["name"], "c/c");
        assert_eq!((all["runs_ok"].as_i64(), all["runs_error"].as_i64()), (Some(2), Some(1)));

        // since со смещением часового пояса приводится к UTC; после последнего
        // снимка изменений нет.
        let later = db.summary(1, Some("2026-01-01T06:30:00+03:00")).unwrap();
        assert_eq!(later["since"], "2026-01-01T03:30:00Z");
        assert_eq!(later["new_repos"], json!([]));
        assert_eq!(later["star_growth"], json!([]));
        assert_eq!(later["fresh_pushes"], json!([]));
        assert_eq!(later["runs_ok"], 0);

        assert!(db.summary(1, Some("вчера")).unwrap_err().contains("RFC3339"));
        assert!(db.summary(9, None).unwrap_err().contains("не найдено"));
    }

    #[test]
    fn list_due_and_delete() {
        let db = fixture_db();
        let list = db.list().unwrap();
        assert_eq!(list[0]["snapshots"], 2);
        assert_eq!(list[0]["last_status"], "ok");
        assert_eq!(list[0]["last_run_at"], "2026-01-01T03:00:00Z");
        // Последний запуск давно — наблюдение просрочено и запускается один раз.
        assert_eq!(db.due().unwrap(), vec![(1, "search language:Rust".to_string())]);
        db.record(1, &db.now().unwrap(), &snapshot(vec![])).unwrap();
        assert!(db.due().unwrap().is_empty());
        let fresh = db.create("new", 5).unwrap();
        assert_eq!(db.due().unwrap(), vec![(fresh, "new".to_string())]);
        assert!(db.delete(1).unwrap());
        assert!(!db.delete(1).unwrap());
        let left: i64 = db.conn().query_row("SELECT COUNT(*) FROM repos_snapshot", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 0, "снимки удаляются вместе с наблюдением");
    }

    #[test]
    fn prune_drops_runs_older_than_retention_with_snapshots() {
        let db = Db::open(":memory:").unwrap();
        let id = db.create("q", 60).unwrap();
        let ago = |days: u32| -> String { db.conn().query_row(
            &format!("SELECT strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-{days} days')"), [], |r| r.get(0)).unwrap() };
        let (old, recent) = (ago(8), ago(6));
        db.record(id, &old, &snapshot(vec![repo("old/one", 1, &old), repo("old/two", 2, &old)])).unwrap();
        db.record(id, &recent, &snapshot(vec![repo("new/one", 3, &recent)])).unwrap();
        assert_eq!(db.prune().unwrap(), 1);
        let runs: Vec<String> = db.conn().prepare("SELECT started_at FROM runs").unwrap()
            .query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(runs, vec![recent]);
        let names: Vec<String> = db.conn().prepare("SELECT full_name FROM repos_snapshot").unwrap()
            .query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(names, vec!["new/one".to_string()], "снимки старого запуска удалены каскадом");
    }

    /// Настоящий HTTP-шов: клиент приложения ↔ Streamable HTTP сервер SDK,
    /// GitHub заменён локальным fixture.
    #[tokio::test]
    async fn mcp_over_http_registers_tools_and_runs_watches() {
        let hits = Arc::new(AtomicUsize::new(0));
        let observed = hits.clone();
        let gh = Router::new().route("/search", get(move |Query(q): Query<HashMap<String, String>>| {
            let observed = observed.clone();
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                assert_eq!(q.get("q").unwrap(), "search language:Rust");
                let per_page = q.get("per_page").unwrap().clone();
                Json(json!({"total_count": 2, "incomplete_results": false, "per_page": per_page, "items": [
                    {"full_name":"fixture/one","html_url":"https://github.com/fixture/one","description":"Test fixture",
                     "language":"Rust","stargazers_count":17,"pushed_at":"2026-01-01T00:00:00Z","archived":false},
                    {"full_name":"fixture/two","html_url":"https://github.com/fixture/two","description":null,
                     "language":"Rust","stargazers_count":3,"pushed_at":"2026-01-02T00:00:00Z","archived":false}]}))
            }
        }));
        let gh_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let github = Arc::new(Github::with_url(&format!("http://{}/search", gh_listener.local_addr().unwrap())).unwrap());
        let gh_server = tokio::spawn(async move { axum::serve(gh_listener, gh).await.unwrap() });
        let db = Arc::new(Db::open(":memory:").unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let app = router(Watcher::new(github.clone(), db.clone()));
        let mcp = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = crate::mcp::connect(&url).await.unwrap();
        let names: Vec<String> = client.list_all_tools().await.unwrap().into_iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, ["search_repositories", "watch_create", "watch_list", "watch_delete", "watch_summary"]);

        for (name, arguments) in [
            ("watch_create", json!({"query":"x","every_minutes":0})),
            ("watch_create", json!({"query":"x","every_minutes":1441})),
            ("watch_create", json!({"query":" ","every_minutes":5})),
            ("watch_create", json!({"query":"x"})),
            ("watch_list", json!({"id":1})),
            ("watch_delete", json!({"id":"1"})),
            ("watch_summary", json!({"id":1,"since":"когда-то"})),
            ("search_repositories", json!({"query":"Rust","limit":6})),
        ] {
            let error = crate::mcp::call(&client, name, arguments.clone()).await;
            assert!(error.is_err(), "{name} {arguments} должен вернуть isError");
        }
        assert_eq!(hits.load(Ordering::SeqCst), 0, "неверные параметры не доходят до GitHub");

        let created = crate::mcp::call(&client, "watch_create",
            json!({"query":" search language:Rust ","every_minutes":1})).await.unwrap();
        assert_eq!(created["id"], 1);
        assert_eq!(created["query"], "search language:Rust");
        assert_eq!(run_due(&github, &db).await.unwrap(), 1);
        assert_eq!(run_due(&github, &db).await.unwrap(), 0, "интервал ещё не прошёл");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        let list = crate::mcp::call(&client, "watch_list", json!({})).await.unwrap();
        assert_eq!(list["watches"][0]["snapshots"], 1);
        assert_eq!(list["watches"][0]["last_status"], "ok");
        let summary = crate::mcp::call(&client, "watch_summary", json!({"id":1})).await.unwrap();
        assert_eq!(summary["repos_in_last_snapshot"], 2);
        assert_eq!(summary["runs_ok"], 1);

        let search = crate::mcp::call(&client, "search_repositories",
            json!({"query":"search language:Rust","limit":1})).await.unwrap();
        assert_eq!(search["repositories"].as_array().unwrap().len(), 1);
        assert_eq!(search["repositories"][0]["stars"], 17);

        // Потолок наблюдений: пятое создаётся, шестое — isError с объяснением.
        for _ in 0..4 {
            crate::mcp::call(&client, "watch_create", json!({"query":"extra","every_minutes":60})).await.unwrap();
        }
        let sixth = crate::mcp::call(&client, "watch_create", json!({"query":"extra","every_minutes":60})).await.unwrap_err();
        assert!(sixth.contains("Не больше 5 наблюдений"), "{sixth}");
        assert_eq!(crate::mcp::call(&client, "watch_list", json!({})).await.unwrap()["watches"].as_array().unwrap().len(), 5);

        assert_eq!(crate::mcp::call(&client, "watch_delete", json!({"id":1})).await.unwrap()["deleted"], 1);
        assert!(crate::mcp::call(&client, "watch_delete", json!({"id":1})).await.unwrap_err().contains("не найдено"));
        assert!(client.call_tool(CallToolRequestParams::new("drop_database")).await.is_err());
        crate::mcp::close(client).await;
        mcp.abort();
        gh_server.abort();
    }
}
