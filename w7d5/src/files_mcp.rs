//! Файлы базы знаний поддержки как MCP-сервер на stdio (`emi mcp-files --data <dir>`) и клиент к нему.
//! ДОПУЩЕНИЕ-1/2: читать можно `faq/**/*.md` и тикеты `crm.json`, писать — только `faq/*.md`, удаления нет.
//! ДОПУЩЕНИЕ-3: правки модели копятся в черновике в памяти сервера. На диск их пишет `apply_draft`,
//! которого модель не видит: его зовёт хост по кнопке оператора «Применить».
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rmcp::{
    model::{CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool},
    service::RequestContext,
    ErrorData, RoleServer, ServerHandler, ServiceExt,
};
use serde_json::{json, Map, Value};
use tokio::time::timeout;

const TIMEOUT: Duration = Duration::from_secs(15);
// crm.json демо — ~15 КБ: целиком влезает, а случайный огромный файл не съест контекст модели.
const MAX_READ: usize = 40_000;
const MAX_HITS: usize = 80;
// Раздел FAQ — абзац-другой; больше — признак того, что модель переписывает файл целиком.
const MAX_WRITE: usize = 20_000;
/// Тулы только для хоста: хост убирает их из каталога для модели и отклоняет, если модель их всё же позовёт.
pub const HOST_ONLY: [&str; 3] = ["draft_diff", "apply_draft", "discard_draft"];

fn schema(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

pub fn tools() -> Vec<Tool> {
    let none = || schema(json!({"type":"object","properties":{},"additionalProperties":false}));
    let text = |d: &str| json!({"type":"string","description":d});
    vec![
        Tool::new("list_files", "Files you can read: knowledge base faq/**/*.md and crm.json (all support tickets, read-only). Files changed in the draft are marked.",
            schema(json!({"type":"object","properties":{"prefix":text("Optional path prefix, e.g. faq/")},"additionalProperties":false}))),
        Tool::new("read_file", "Read one file. If the file has unsaved draft changes, the draft version is returned.",
            schema(json!({"type":"object","properties":{"path":text("Relative path, e.g. faq/payments.md or crm.json")},"required":["path"],"additionalProperties":false}))),
        Tool::new("search", "Case-insensitive substring search over all readable files; returns `path:line: text`.",
            schema(json!({"type":"object","properties":{
                "query":text("Word or phrase, e.g. роутер"),
                "prefix":text("Optional path prefix to narrow the search, e.g. faq/")},
                "required":["query"],"additionalProperties":false}))),
        Tool::new("edit_file", "Change an existing faq/*.md file in the draft: replaces exactly one occurrence of `old` with `new`. Copy `old` verbatim from read_file. Nothing is written to disk until the operator approves.",
            schema(json!({"type":"object","properties":{
                "path":text("faq/<name>.md"),
                "old":text("Exact fragment of the current file, must occur exactly once"),
                "new":text("Replacement text")},
                "required":["path","old","new"],"additionalProperties":false}))),
        Tool::new("create_file", "Create a new faq/<name>.md file in the draft (the file must not exist). Nothing is written to disk until the operator approves.",
            schema(json!({"type":"object","properties":{
                "path":text("faq/<name>.md, latin letters, e.g. faq/router.md"),
                "content":text("Full markdown content starting with `# Title`")},
                "required":["path","content"],"additionalProperties":false}))),
        Tool::new("draft_diff", "Host only: unified diff of all draft changes against disk.", none()),
        Tool::new("apply_draft", "Host only: write all draft changes to disk and clear the draft.", none()),
        Tool::new("discard_draft", "Host only: drop all draft changes.", none()),
    ]
}

fn arg<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    args.get(key).and_then(Value::as_str).ok_or(format!("нужна строка {key}"))
}

/// Аргументы от модели — непроверенный вход: только относительный путь внутри папки данных,
/// без `..`, `.` и пустых частей, не похожий на флаг.
fn rel_path(args: &Map<String, Value>, key: &str) -> Result<String, String> {
    let p = arg(args, key)?.trim();
    let bad = p.is_empty() || p.starts_with('-') || p.starts_with('/') || p.contains('\\') || p.contains('\0') || p.split('/').any(|c| c.is_empty() || c == "." || c == "..");
    if bad {
        return Err(format!("{key}: нужен относительный путь внутри папки данных без «..», например faq/payments.md"));
    }
    Ok(p.to_string())
}

fn readable(path: &str) -> bool {
    path == "crm.json" || (path.starts_with("faq/") && path.ends_with(".md"))
}

/// Писать — только файлы прямо в `faq/`: индекс FAQ берёт `*.md` без подпапок, файл глубже никто не увидит.
/// `specialist.md` — правила передачи специалисту: чат держит его в памяти с запуска, и меняет его человек, а не ассистент.
fn writable(path: &str) -> bool {
    path.strip_prefix("faq/").is_some_and(|name| {
        !name.contains('/') && !name.starts_with('.') && name.len() > 3 && name.ends_with(".md") && name != "specialist.md"
    })
}

fn short(text: &str, max: usize) -> String {
    if text.chars().count() <= max { text.to_string() } else { format!("{}…", text.chars().take(max).collect::<String>()) }
}

fn collect_md(dir: &Path, rel: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let (Ok(kind), Ok(name)) = (entry.file_type(), entry.file_name().into_string()) else { continue };
        let path = format!("{rel}/{name}");
        // file_type не идёт по симлинкам: ссылка не файл и не папка — пропускаем, наружу через неё не выйти.
        if kind.is_dir() {
            collect_md(&entry.path(), &path, out);
        } else if kind.is_file() && path.ends_with(".md") {
            out.push(path);
        }
    }
}

pub struct FilesServer {
    /// Канонический путь папки данных: с ним сравнивается реальный путь каждого читаемого файла.
    root: PathBuf,
    draft: Mutex<BTreeMap<String, String>>,
}

impl FilesServer {
    pub fn new(data: &Path) -> Result<FilesServer, String> {
        let root = data.canonicalize().map_err(|e| format!("нет папки данных {} ({e})", data.display()))?;
        Ok(FilesServer { root, draft: Mutex::new(BTreeMap::new()) })
    }

    /// Файл с диска; `None` — файла нет. Симлинк в любой части пути — отказ: иначе `faq/x.md` мог бы вести на `.env`.
    fn disk(&self, path: &str) -> Result<Option<String>, String> {
        let full = self.root.join(path);
        if !full.exists() {
            return Ok(None);
        }
        if full.canonicalize().ok().as_ref() != Some(&full) {
            return Err(format!("{path}: путь ведёт за пределы папки данных"));
        }
        std::fs::read_to_string(&full).map(Some).map_err(|e| format!("не прочитать {path}: {e}"))
    }

    fn current(&self, draft: &BTreeMap<String, String>, path: &str) -> Result<Option<String>, String> {
        match draft.get(path) {
            Some(text) => Ok(Some(text.clone())),
            None => self.disk(path),
        }
    }

    fn all_paths(&self, draft: &BTreeMap<String, String>) -> Vec<String> {
        let mut paths = Vec::new();
        collect_md(&self.root.join("faq"), "faq", &mut paths);
        if self.root.join("crm.json").is_file() {
            paths.push("crm.json".into());
        }
        paths.extend(draft.keys().filter(|k| !paths.contains(k)).cloned().collect::<Vec<_>>());
        paths.sort();
        paths
    }

    fn run(&self, name: &str, args: &Map<String, Value>) -> Result<String, String> {
        let mut draft = self.draft.lock().map_err(|_| "черновик повреждён паникой — перезапусти Эми")?;
        let prefix = match args.get("prefix").and_then(Value::as_str).map(str::trim) {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => String::new(),
        };
        match name {
            "list_files" => {
                let mut out = Vec::new();
                for path in self.all_paths(&draft).into_iter().filter(|p| p.starts_with(&prefix)) {
                    let mark = match (draft.contains_key(&path), self.root.join(&path).exists()) {
                        (true, true) => " (изменён в черновике)",
                        (true, false) => " (новый, в черновике)",
                        _ => "",
                    };
                    out.push(format!("{path}{mark}"));
                }
                Ok(if out.is_empty() { "файлов не найдено".into() } else { out.join("\n") })
            }
            "read_file" => {
                let path = rel_path(args, "path")?;
                if !readable(&path) {
                    return Err(format!("{path}: читать можно только faq/*.md и crm.json"));
                }
                let text = self.current(&draft, &path)?.ok_or(format!("{path}: файла нет — список файлов даёт list_files"))?;
                let total = text.chars().count();
                let text = if total > MAX_READ { format!("{}\n… [обрезано, всего {total} символов]", short(&text, MAX_READ)) } else { text };
                // Метка черновика: модель не должна принять собственную правку за данные из базы знаний.
                Ok(if draft.contains_key(&path) { format!("[черновик, ещё не записан на диск]\n{text}") } else { text })
            }
            "search" => {
                let query = arg(args, "query")?.trim().to_lowercase();
                if query.chars().count() < 2 {
                    return Err("query: нужно хотя бы 2 символа".into());
                }
                let mut hits = Vec::new();
                for path in self.all_paths(&draft).into_iter().filter(|p| p.starts_with(&prefix)) {
                    let Ok(Some(text)) = self.current(&draft, &path) else { continue };
                    for (n, line) in text.lines().enumerate().filter(|(_, l)| l.to_lowercase().contains(&query)) {
                        hits.push(format!("{path}:{}: {}", n + 1, short(line.trim(), 200)));
                    }
                }
                let total = hits.len();
                hits.truncate(MAX_HITS);
                if total > MAX_HITS {
                    hits.push(format!("… ещё {} совпадений — уточни query или prefix", total - MAX_HITS));
                }
                Ok(if hits.is_empty() { format!("«{query}» не найдено") } else { hits.join("\n") })
            }
            "edit_file" | "create_file" => {
                let path = rel_path(args, "path")?;
                if !writable(&path) {
                    return Err(format!("{path}: менять можно только файлы faq/<имя>.md (кроме specialist.md)"));
                }
                let current = self.current(&draft, &path)?;
                let next = if name == "edit_file" {
                    let (old, new) = (arg(args, "old")?, arg(args, "new")?);
                    let current = current.ok_or(format!("{path}: файла нет — новый файл создаёт create_file"))?;
                    if old.is_empty() || new.chars().count() > MAX_WRITE {
                        return Err(format!("old не может быть пустым, new — не длиннее {MAX_WRITE} символов"));
                    }
                    match current.matches(old).count() {
                        0 => return Err(format!("{path}: фрагмент old не найден — перечитай файл read_file и скопируй фрагмент дословно")),
                        1 => current.replacen(old, new, 1),
                        n => return Err(format!("{path}: фрагмент old встречается {n} раз — добавь соседние строки, чтобы он был уникален")),
                    }
                } else {
                    if current.is_some() {
                        return Err(format!("{path}: файл уже есть — меняй его через edit_file"));
                    }
                    let content = arg(args, "content")?;
                    if content.trim().is_empty() || content.chars().count() > MAX_WRITE {
                        return Err(format!("content: нужен непустой текст не длиннее {MAX_WRITE} символов"));
                    }
                    content.to_string()
                };
                draft.insert(path.clone(), next);
                Ok(format!("{path}: правка в черновике, на диск не записана — запишет оператор после просмотра diff."))
            }
            "draft_diff" => self.diff(&draft),
            "apply_draft" => {
                if draft.is_empty() {
                    return Err("черновик пуст — записывать нечего".into());
                }
                let mut written = Vec::new();
                let paths: Vec<String> = draft.keys().cloned().collect();
                for path in paths {
                    let full = self.root.join(&path);
                    // tmp + rename: индекс и чат читают FAQ параллельно и не должны увидеть файл записанным наполовину.
                    let tmp = full.with_extension("md.tmp");
                    let done = if writable(&path) { std::fs::write(&tmp, &draft[&path]).and_then(|_| std::fs::rename(&tmp, &full)).map_err(|e| e.to_string()) } else { Err("путь вне faq/*.md".into()) };
                    if let Err(e) = done {
                        let _ = std::fs::remove_file(&tmp);
                        for w in &written {
                            draft.remove(w);
                        }
                        return Err(format!("не записать {path}: {e}; уже записано: {}", if written.is_empty() { "ничего".into() } else { written.join(", ") }));
                    }
                    written.push(path);
                }
                draft.clear();
                Ok(format!("записано: {}", written.join(", ")))
            }
            "discard_draft" => {
                draft.clear();
                Ok("черновик очищен".into())
            }
            other => Err(format!("неизвестный инструмент {other}")),
        }
    }

    /// Unified diff черновика против диска через `git diff --no-index` на временных копиях — без своей реализации diff.
    fn diff(&self, draft: &BTreeMap<String, String>) -> Result<String, String> {
        if draft.is_empty() {
            return Ok(String::new());
        }
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let tmp = std::env::temp_dir().join(format!("emi-draft-{}-{nanos}", std::process::id()));
        let result = (|| {
            let mut out = String::new();
            for (path, text) in draft {
                let (a, b) = (format!("a/{path}"), format!("b/{path}"));
                let old = match self.disk(path)? {
                    Some(old) => {
                        write_tmp(&tmp.join(&a), &old)?;
                        a
                    }
                    None => "/dev/null".to_string(),
                };
                write_tmp(&tmp.join(&b), text)?;
                let run = Command::new("git")
                    .args(["-c", "core.quotepath=false", "diff", "--no-index", "--no-prefix", "--no-color", "--no-ext-diff", "--"])
                    .args([&old, &b])
                    .current_dir(&tmp)
                    .stdin(Stdio::null())
                    .output()
                    .map_err(|e| format!("git не запустился: {e}"))?;
                // --no-index отвечает кодом 1, когда отличия есть, — это не ошибка.
                match run.status.code() {
                    Some(0 | 1) => out.push_str(&String::from_utf8_lossy(&run.stdout)),
                    _ => return Err(format!("git diff {path}: {}", String::from_utf8_lossy(&run.stderr).trim())),
                }
            }
            Ok(out)
        })();
        let _ = std::fs::remove_dir_all(&tmp);
        result
    }
}

fn write_tmp(file: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("временная папка diff: {e}"))?;
    }
    std::fs::write(file, text).map_err(|e| format!("временная папка diff: {e}"))
}

impl ServerHandler for FilesServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("emi-files", env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(&self, _: Option<PaginatedRequestParams>, _: RequestContext<RoleServer>) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    async fn call_tool(&self, request: CallToolRequestParams, _: RequestContext<RoleServer>) -> Result<CallToolResponse, ErrorData> {
        if !tools().iter().any(|t| t.name == request.name) {
            return Err(ErrorData::invalid_params(format!("неизвестный инструмент {}", request.name), None));
        }
        Ok(match self.run(&request.name, &request.arguments.unwrap_or_default()) {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(error) => CallToolResult::error(vec![ContentBlock::text(error)]),
        }
        .into())
    }
}

pub async fn serve(data: PathBuf) -> Result<(), String> {
    // stdout занят протоколом MCP; сообщения приложения сюда не пишем.
    let service = FilesServer::new(&data)?.serve(rmcp::transport::stdio()).await.map_err(|e| e.to_string())?;
    service.waiting().await.map_err(|e| e.to_string())?;
    Ok(())
}

pub type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

pub async fn connect(data: &Path) -> Result<Client, String> {
    let exe = std::env::current_exe().map_err(|e| format!("не найти свой бинарник: {e}"))?;
    let mut command = tokio::process::Command::new(exe);
    command.arg("mcp-files").arg("--data").arg(data).kill_on_drop(true);
    let transport = rmcp::transport::TokioChildProcess::new(command).map_err(|e| format!("MCP-файлы не запустился: {e}"))?;
    timeout(Duration::from_secs(5), ().serve(transport))
        .await
        .map_err(|_| "MCP-файлы не ответил за 5 секунд".to_string())?
        .map_err(|e| format!("MCP-файлы: {e}"))
}

/// Каталог для модели: без тулов хоста — записать на диск или подсмотреть diff она не может.
pub async fn openai_tools(client: &Client) -> Result<Vec<Value>, String> {
    let tools = timeout(TIMEOUT, client.list_all_tools())
        .await
        .map_err(|_| "MCP-файлы: каталог не получен за 15 секунд".to_string())?
        .map_err(|e| format!("MCP-файлы: каталог: {e}"))?;
    Ok(tools
        .into_iter()
        .filter(|t| !HOST_ONLY.contains(&t.name.as_ref()))
        .map(|t| json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":*t.input_schema}}))
        .collect())
}

pub async fn call(client: &Client, name: &str, args: Map<String, Value>) -> Result<String, String> {
    let params = CallToolRequestParams::new(name.to_string()).with_arguments(args);
    let result = timeout(TIMEOUT, client.call_tool(params))
        .await
        .map_err(|_| format!("MCP-файлы: {name} не ответил"))?
        .map_err(|e| format!("MCP-файлы: {name}: {e}"))?;
    let text: String = result.content.iter().filter_map(|c| c.as_text().map(|t| t.text.as_str())).collect::<Vec<_>>().join("\n");
    match result.is_error {
        Some(true) => Err(text),
        _ => Ok(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> Map<String, Value> {
        schema(v)
    }

    #[tokio::test]
    async fn files_tools_read_search_and_change_only_draft_until_apply() {
        let dir = std::env::temp_dir().join(format!("emi-files-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("faq")).unwrap();
        let payments = "# Оплата\n\n## Способы оплаты\nКарты и СБП.\n";
        let crm = r#"{"tickets": [{"id": "T-1003", "messages": [{"text": "Как настроить VPN на роутере?"}]}]}"#;
        for (path, text) in [("faq/payments.md", payments), ("faq/plans.md", "# Тарифы\nСтарт и Год.\n"), ("crm.json", crm), ("crm.seed.json", "{}"), (".env", "GEMINI_API_KEY=secret")] {
            std::fs::write(dir.join(path), text).unwrap();
        }
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let server_dir = dir.clone();
        let server = tokio::spawn(async move { FilesServer::new(&server_dir).unwrap().serve(server_io).await.unwrap().waiting().await.unwrap() });
        let client = ().serve(client_io).await.unwrap();
        let tool = |name: &'static str, a: Value| {
            let client = &client;
            async move { call(client, name, args(a)).await }
        };

        // Модель не видит тулов хоста.
        let names: Vec<String> = openai_tools(&client).await.unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, ["list_files", "read_file", "search", "edit_file", "create_file"]);

        // Список и поиск — по нескольким файлам, секреты и сид в них не попадают.
        assert_eq!(tool("list_files", json!({})).await.unwrap(), "crm.json\nfaq/payments.md\nfaq/plans.md");
        let hits = tool("search", json!({"query": "СБП"})).await.unwrap();
        assert_eq!(hits, "faq/payments.md:4: Карты и СБП.");
        let hits = tool("search", json!({"query": "роутер"})).await.unwrap();
        assert!(hits.starts_with("crm.json:1: "), "{hits}");
        assert_eq!(tool("search", json!({"query": "секрет"})).await.unwrap(), "«секрет» не найдено");

        // Выход за папку, абсолютные пути, флаги, .env и сид — отказ. Файл снаружи существует: отказ не из-за «файла нет».
        let outside = dir.with_file_name(format!("emi-files-outside-{}.md", std::process::id()));
        std::fs::write(&outside, "секрет снаружи").unwrap();
        let escape = format!("faq/../../{}", outside.file_name().unwrap().to_str().unwrap());
        for path in [escape.as_str(), "../w7d3/data/crm.json", "/etc/passwd", "faq/../.env", ".env", "crm.seed.json", "-faq.md", "faq//plans.md"] {
            assert!(tool("read_file", json!({"path": path})).await.is_err(), "{path} должен быть отклонён");
        }
        // crm.json — только чтение, писать можно только faq/*.md.
        assert!(tool("read_file", json!({"path": "crm.json"})).await.unwrap().contains("T-1003"));
        let err = tool("edit_file", json!({"path": "crm.json", "old": "T-1003", "new": "T-1"})).await.unwrap_err();
        assert!(err.contains("только файлы faq/"), "{err}");
        assert!(tool("create_file", json!({"path": "faq/sub/x.md", "content": "# X"})).await.is_err());
        assert!(tool("create_file", json!({"path": "../evil.md", "content": "# X"})).await.is_err());
        // На macOS faq/Payments.md — это faq/payments.md: disk() видит расхождение канонического пути, create_file не перезапишет файл.
        assert!(tool("create_file", json!({"path": "faq/Payments.md", "content": "# X"})).await.is_err());
        assert!(tool("create_file", json!({"path": "faq/specialist.md", "content": "# X"})).await.is_err());

        // Замена ровно одного вхождения: 0 и >1 — понятная ошибка.
        assert!(tool("edit_file", json!({"path": "faq/payments.md", "old": "Наличные", "new": "x"})).await.unwrap_err().contains("не найден"));
        assert!(tool("edit_file", json!({"path": "faq/payments.md", "old": "плат", "new": "x"})).await.unwrap_err().contains("встречается 2 раз"));

        // Правка и новый файл попадают в черновик, диск не меняется.
        tool("edit_file", json!({"path": "faq/payments.md", "old": "Карты и СБП.\n", "new": "Карты и СБП.\n\n## Оплата из-за границы\nНапишите специалисту.\n"})).await.unwrap();
        tool("create_file", json!({"path": "faq/router.md", "content": "# Роутер\nНапишите специалисту.\n"})).await.unwrap();
        assert!(tool("create_file", json!({"path": "faq/router.md", "content": "# Снова"})).await.is_err());
        assert_eq!(std::fs::read_to_string(dir.join("faq/payments.md")).unwrap(), payments);
        assert!(!dir.join("faq/router.md").exists());
        assert!(tool("read_file", json!({"path": "faq/payments.md"})).await.unwrap().starts_with("[черновик"));
        assert!(tool("list_files", json!({"prefix": "faq/"})).await.unwrap().contains("faq/router.md (новый, в черновике)"));

        // Diff — по всем файлам черновика, с путями faq/*.md.
        let diff = tool("draft_diff", json!({})).await.unwrap();
        assert!(diff.contains("--- a/faq/payments.md\n+++ b/faq/payments.md") && diff.contains("+## Оплата из-за границы"), "{diff}");
        assert!(diff.contains("--- /dev/null\n+++ b/faq/router.md") && diff.contains("+# Роутер"), "{diff}");

        // Применение пишет на диск и очищает черновик.
        assert_eq!(tool("apply_draft", json!({})).await.unwrap(), "записано: faq/payments.md, faq/router.md");
        assert!(std::fs::read_to_string(dir.join("faq/payments.md")).unwrap().contains("## Оплата из-за границы"));
        assert_eq!(std::fs::read_to_string(dir.join("faq/router.md")).unwrap(), "# Роутер\nНапишите специалисту.\n");
        assert_eq!(tool("draft_diff", json!({})).await.unwrap(), "");
        assert!(tool("apply_draft", json!({})).await.is_err());

        // Сброс черновика хостом перед новой задачей.
        tool("create_file", json!({"path": "faq/tmp.md", "content": "# Временно"})).await.unwrap();
        tool("discard_draft", json!({})).await.unwrap();
        assert_eq!(tool("draft_diff", json!({})).await.unwrap(), "");

        client.cancel().await.unwrap();
        server.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&outside);
    }
}
