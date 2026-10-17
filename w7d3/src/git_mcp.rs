//! Read-only git как MCP-сервер на stdio (`emi mcp-git --repo <path>`) и клиент к нему.
//! Сервер — тот же бинарник, дочерний процесс: модель видит git только через этот каталог тулов.
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rmcp::{
    model::{CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool},
    service::RequestContext,
    ErrorData, RoleServer, ServerHandler, ServiceExt,
};
use serde_json::{json, Map, Value};
use tokio::time::timeout;

// Модель читает результат целиком — большой diff съест контекст и не даст ответить.
const MAX_OUTPUT: usize = 12_000;
const MAX_FILES: usize = 400;
const TIMEOUT: Duration = Duration::from_secs(15);

fn schema(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

pub fn tools() -> Vec<Tool> {
    let none = || schema(json!({"type":"object","properties":{},"additionalProperties":false}));
    vec![
        Tool::new("git_branch", "Current branch of the project repository and its upstream.", none()),
        Tool::new("git_status", "Uncommitted changes: `git status --short --branch`.", none()),
        Tool::new("git_log", "Recent commits, one line each: short hash, date, author, subject.",
            schema(json!({"type":"object","properties":{
                "limit":{"type":"integer","minimum":1,"maximum":30,"description":"How many commits, 1–30 (default 10)"}},
                "additionalProperties":false}))),
        Tool::new("git_diff", "Diff of uncommitted changes to tracked files (working tree vs HEAD), optionally for one path. New untracked files are not included — see git_status. Output is truncated.",
            schema(json!({"type":"object","properties":{
                "path":{"type":"string","description":"Optional file or directory inside the repository"}},
                "additionalProperties":false}))),
        Tool::new("list_files", "Tracked files of the repository, optionally under a directory prefix.",
            schema(json!({"type":"object","properties":{
                "prefix":{"type":"string","description":"Optional directory prefix, e.g. w6d5/"}},
                "additionalProperties":false}))),
    ]
}

/// Аргументы от модели — непроверенный вход: путь не может начинаться с `-`
/// (иначе git примет его за флаг) и выходить из репозитория.
fn safe_path(args: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.starts_with('-') || s.starts_with('/') || s.split('/').any(|p| p == "..") {
                return Err(format!("{key}: нужен относительный путь внутри репозитория"));
            }
            Ok(Some(s.to_string()))
        }
        Some(_) => Err(format!("{key} должен быть строкой")),
    }
}

async fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    // --no-optional-locks: чтение не должно мешать параллельному git пользователя.
    let run = tokio::process::Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = timeout(TIMEOUT, run)
        .await
        .map_err(|_| format!("git {} не ответил за {} с", args[0], TIMEOUT.as_secs()))?
        .map_err(|e| format!("git не запустился: {e}"))?;
    if !out.status.success() {
        return Err(format!("git {}: {}", args[0], String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn cut(text: String) -> String {
    if text.len() <= MAX_OUTPUT {
        return text;
    }
    let mut end = MAX_OUTPUT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… [обрезано: показано {end} из {} байт]", &text[..end], text.len())
}

pub async fn run_tool(repo: &Path, name: &str, args: &Map<String, Value>) -> Result<String, String> {
    match name {
        "git_branch" => {
            let branch = git(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
            let upstream = git(repo, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]).await.unwrap_or_default();
            let upstream = if upstream.trim().is_empty() { "нет".to_string() } else { upstream.trim().to_string() };
            Ok(format!("current branch: {}\nupstream: {upstream}", branch.trim()))
        }
        "git_status" => Ok(cut(git(repo, &["status", "--short", "--branch"]).await?)),
        "git_log" => {
            let limit = match args.get("limit") {
                None | Some(Value::Null) => 10,
                Some(v) => v.as_u64().filter(|n| (1..=30).contains(n)).ok_or("limit должен быть целым 1–30")?,
            };
            let n = format!("-{limit}");
            Ok(cut(git(repo, &["log", &n, "--date=short", "--pretty=format:%h %ad %an: %s"]).await?))
        }
        "git_diff" => {
            let mut a = vec!["diff", "HEAD", "--stat", "--patch", "--"];
            let path = safe_path(args, "path")?;
            if let Some(p) = &path {
                a.push(p);
            }
            let diff = git(repo, &a).await?;
            Ok(if diff.trim().is_empty() { "нет незакоммиченных изменений".into() } else { cut(diff) })
        }
        "list_files" => {
            let mut a = vec!["ls-files", "--"];
            let prefix = safe_path(args, "prefix")?;
            if let Some(p) = &prefix {
                a.push(p);
            }
            let files = git(repo, &a).await?;
            let all: Vec<&str> = files.lines().collect();
            let mut text = all.iter().take(MAX_FILES).copied().collect::<Vec<_>>().join("\n");
            if all.len() > MAX_FILES {
                text.push_str(&format!("\n… ещё {} файлов, сузь prefix", all.len() - MAX_FILES));
            }
            Ok(if text.is_empty() { "файлов не найдено".into() } else { text })
        }
        other => Err(format!("неизвестный инструмент {other}")),
    }
}

pub struct GitServer {
    repo: PathBuf,
}

impl ServerHandler for GitServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("emi-git", env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(&self, _: Option<PaginatedRequestParams>, _: RequestContext<RoleServer>) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    async fn call_tool(&self, request: CallToolRequestParams, _: RequestContext<RoleServer>) -> Result<CallToolResponse, ErrorData> {
        if !tools().iter().any(|t| t.name == request.name) {
            return Err(ErrorData::invalid_params(format!("неизвестный инструмент {}", request.name), None));
        }
        let args = request.arguments.unwrap_or_default();
        Ok(match run_tool(&self.repo, &request.name, &args).await {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(error) => CallToolResult::error(vec![ContentBlock::text(error)]),
        }
        .into())
    }
}

pub async fn serve(repo: PathBuf) -> Result<(), String> {
    // stdout занят протоколом MCP; сообщения приложения сюда не пишем.
    let service = GitServer { repo }.serve(rmcp::transport::stdio()).await.map_err(|e| e.to_string())?;
    service.waiting().await.map_err(|e| e.to_string())?;
    Ok(())
}

pub type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

pub async fn connect(repo: &Path) -> Result<Client, String> {
    let exe = std::env::current_exe().map_err(|e| format!("не найти свой бинарник: {e}"))?;
    let mut command = tokio::process::Command::new(exe);
    command.arg("mcp-git").arg("--repo").arg(repo).kill_on_drop(true);
    let transport = rmcp::transport::TokioChildProcess::new(command).map_err(|e| format!("MCP-git не запустился: {e}"))?;
    timeout(Duration::from_secs(5), ().serve(transport))
        .await
        .map_err(|_| "MCP-git не ответил за 5 секунд".to_string())?
        .map_err(|e| format!("MCP-git: {e}"))
}

/// Каталог MCP в формате tools OpenAI Chat Completions.
pub async fn openai_tools(client: &Client) -> Result<Vec<Value>, String> {
    let tools = timeout(TIMEOUT, client.list_all_tools())
        .await
        .map_err(|_| "MCP-git: каталог не получен за 15 секунд".to_string())?
        .map_err(|e| format!("MCP-git: каталог: {e}"))?;
    Ok(tools
        .into_iter()
        .map(|t| json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":*t.input_schema}}))
        .collect())
}

/// Ошибка инструмента — тоже текст для модели: она может поправить аргументы.
pub async fn call(client: &Client, name: &str, args: Map<String, Value>) -> Result<String, String> {
    let params = CallToolRequestParams::new(name.to_string()).with_arguments(args);
    let result = timeout(TIMEOUT * 2, client.call_tool(params))
        .await
        .map_err(|_| format!("MCP-git: {name} не ответил"))?
        .map_err(|e| format!("MCP-git: {name}: {e}"))?;
    let text: String = result.content.iter().filter_map(|c| c.as_text().map(|t| t.text.as_str())).collect::<Vec<_>>().join("\n");
    match result.is_error {
        Some(true) => Err(text),
        _ => Ok(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(["-c", "user.name=Test", "-c", "user.email=t@example.com", "-c", "init.defaultBranch=main"])
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    fn temp_repo() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("emi-git-test-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        sh(&dir, &["init", "-q"]);
        std::fs::write(dir.join("README.md"), "# Проект\nпервая строка\n").unwrap();
        std::fs::write(dir.join("docs/guide.md"), "# Гайд\n").unwrap();
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-q", "-m", "feat: первый коммит"]);
        sh(&dir, &["checkout", "-q", "-b", "feature-x"]);
        std::fs::write(dir.join("README.md"), "# Проект\nвторая строка\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn git_tools_over_mcp_on_temp_repo() {
        let repo = temp_repo();
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let server_repo = repo.clone();
        let server = tokio::spawn(async move {
            GitServer { repo: server_repo }.serve(server_io).await.unwrap().waiting().await.unwrap();
        });
        let client = ().serve(client_io).await.unwrap();

        let names: Vec<String> = openai_tools(&client).await.unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, ["git_branch", "git_status", "git_log", "git_diff", "list_files"]);

        let none = Map::new;
        let branch = call(&client, "git_branch", none()).await.unwrap();
        assert!(branch.starts_with("current branch: feature-x"), "{branch}");
        let status = call(&client, "git_status", none()).await.unwrap();
        assert!(status.contains(" M README.md"), "{status}");
        let log = call(&client, "git_log", schema(json!({"limit": 5}))).await.unwrap();
        assert_eq!(log.lines().count(), 1);
        assert!(log.ends_with("Test: feat: первый коммит"), "{log}");
        let diff = call(&client, "git_diff", schema(json!({"path": "README.md"}))).await.unwrap();
        assert!(diff.contains("+вторая строка") && diff.contains("-первая строка"), "{diff}");
        let files = call(&client, "list_files", schema(json!({"prefix": "docs/"}))).await.unwrap();
        assert_eq!(files, "docs/guide.md");

        // Аргументы-флаги и выход из репозитория отклоняются до запуска git.
        for (tool, args) in [("git_diff", json!({"path": "--output=/tmp/x"})), ("list_files", json!({"prefix": "../"})), ("git_log", json!({"limit": 0}))] {
            assert!(call(&client, tool, schema(args)).await.is_err(), "{tool}");
        }
        assert!(call(&client, "git_push", none()).await.is_err());

        client.cancel().await.unwrap();
        server.await.unwrap();
        let _ = std::fs::remove_dir_all(&repo);
    }
}
