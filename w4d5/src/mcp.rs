//! Клиенты собственного MCP-сервера наблюдений (Streamable HTTP) и публичного каталога Exa.
use std::time::Duration;

use rmcp::{
    model::{ClientCapabilities, ClientConfig, Implementation, Tool},
    transport::StreamableHttpClientTransport,
    ClientLifecycleMode, ClientServiceExt,
};
use serde_json::{json, Value};
use tokio::time::timeout;

pub const EXA_URL: &str = "https://mcp.exa.ai/mcp";
const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ToolTrace {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    pub arguments: Value,
    pub result: Option<Value>,
}

pub const DEFAULT_WATCH_URL: &str = "http://127.0.0.1:8800/mcp";

pub struct Server {
    pub name: &'static str,
    pub env: &'static str,
    pub default_url: &'static str,
    pub allow: Option<&'static [&'static str]>,
}

pub const SERVERS: [Server; 3] = [
    Server { name: "research", env: "WATCH_MCP_URL", default_url: DEFAULT_WATCH_URL, allow: None },
    Server { name: "deepwiki", env: "DEEPWIKI_MCP_URL", default_url: "https://mcp.deepwiki.com/mcp", allow: Some(&["ask_wiki_question"]) },
    Server { name: "notify", env: "NOTIFY_MCP_URL", default_url: "http://127.0.0.1:8800/notify", allow: None },
];

pub fn server_url(server: &Server) -> String {
    std::env::var(server.env).ok().filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| server.default_url.into())
}

pub fn merge_catalogs(listed: Vec<(&str, Vec<Tool>)>) -> Vec<(String, Tool)> {
    listed.into_iter().flat_map(|(name, tools)| {
        let server = SERVERS.iter().find(|s| s.name == name);
        tools.into_iter().filter(move |tool| server.is_some_and(|s|
            s.allow.is_none_or(|allow| allow.contains(&tool.name.as_ref()))))
            .map(move |tool| (name.to_string(), tool))
    }).collect()
}

pub type Client = rmcp::service::RunningService<rmcp::RoleClient, ClientConfig>;

/// Адрес собственного MCP-сервера наблюдений (`--mcp-server`).
pub fn watch_url() -> String {
    server_url(&SERVERS[0])
}

pub async fn connect(url: &str) -> Result<Client, String> {
    let transport = StreamableHttpClientTransport::from_uri(url.to_owned());
    let info = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("w4d5-researcher", env!("CARGO_PKG_VERSION")),
    );
    timeout(
        TIMEOUT,
        info.serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Initialize,
        ),
    )
    .await
    .map_err(|_| "MCP: сервер не ответил при подключении за 20 секунд".to_string())?
    .map_err(|e| format!("MCP: не удалось подключиться к {url}: {e}"))
}

pub async fn close(mut client: Client) {
    let _ = client.close_with_timeout(Duration::from_secs(3)).await;
}

/// Вызов инструмента: структурированный результат или текст ошибки (`isError`).
pub async fn call(client: &Client, name: &str, arguments: Value) -> Result<Value, String> {
    let params = rmcp::model::CallToolRequestParams::new(name.to_string())
        .with_arguments(arguments.as_object().cloned().unwrap_or_default());
    let result = timeout(TIMEOUT, client.call_tool(params)).await
        .map_err(|_| format!("MCP: {name} не ответил за 20 секунд"))?
        .map_err(|e| format!("MCP: {name}: {e}"))?;
    let data = result.structured_content.clone().unwrap_or_else(|| json!(result.content));
    match result.is_error {
        Some(true) => Err(data["error"].as_str().map_or_else(|| data.to_string(), str::to_string)),
        _ => Ok(data),
    }
}

/// CLI: каталог сервера наблюдений или один поиск GitHub через него.
pub async fn github_cli(query: Option<String>) -> Result<Value, String> {
    let client = connect(&watch_url()).await?;
    let result = match query {
        Some(query) => call(&client, crate::github::TOOL, json!({"query": query, "limit": 3})).await
            .map_err(|e| format!("Поиск не выполнен: {e}")),
        None => timeout(TIMEOUT, client.list_all_tools()).await
            .map_err(|_| "Тайм-аут каталога MCP".to_string())
            .and_then(|r| r.map_err(|e| e.to_string()))
            .map(|tools| json!(tools)),
    };
    close(client).await;
    result
}

pub async fn discover(url: &str) -> Result<Value, String> {
    let mut client = connect(url).await?;
    let server = client.peer_info();
    // SDK запрашивает следующие страницы, если сервер вернул nextCursor.
    let result = timeout(TIMEOUT, client.list_all_tools()).await;
    // Закрываем соединение и при ошибке tools/list, до возврата из функции.
    let closed = client.close_with_timeout(Duration::from_secs(3)).await;
    let tools = result
        .map_err(|_| "MCP: список инструментов не получен за 20 секунд".to_string())?
        .map_err(|e| format!("MCP: не удалось получить инструменты: {e}"))?;
    match closed {
        Ok(Some(_)) => {}
        _ => return Err("MCP: каталог получен, но завершение соединения не подтверждено".into()),
    }
    Ok(json!({ "url": url, "server": server.as_deref(), "tools": tools }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::StatusCode, response::IntoResponse, routing::post, Json, Router};
    use std::sync::{Arc, Mutex};

    #[test]
    fn catalogs_preserve_server_order_and_allowlist() {
        let tools = |names: &[&str]| names.iter().map(|name|
            Tool::new(name.to_string(), "fixture", serde_json::Map::new())).collect();
        let research = ["search_repositories", "summarize", "save_to_file", "watch_list", "watch_create", "watch_delete", "watch_summary"];
        let wiki = ["ask_wiki_question", "read_wiki_contents", "read_wiki_structure"];
        let notify = ["send_telegram"];
        let merged = merge_catalogs(vec![
            ("research", tools(&research)), ("deepwiki", tools(&wiki)), ("notify", tools(&notify)),
        ]);
        let pairs: Vec<_> = merged.iter().map(|(s, t)| (s.as_str(), t.name.as_ref())).collect();
        assert_eq!(pairs, research.iter().map(|n| ("research", *n))
            .chain([("deepwiki", "ask_wiki_question"), ("notify", "send_telegram")]).collect::<Vec<_>>());
        assert_eq!(pairs.len(), 9);
        let partial = merge_catalogs(vec![("research", tools(&research)),
            ("unknown", tools(&["unexpected"])), ("notify", tools(&notify))]);
        let partial_pairs: Vec<_> = partial.iter().map(|(s, t)| (s.as_str(), t.name.as_ref())).collect();
        assert_eq!(partial_pairs, research.iter().map(|n| ("research", *n))
            .chain([("notify", "send_telegram")]).collect::<Vec<_>>());
    }

    #[test]
    fn old_traces_have_no_server_and_none_is_omitted() {
        let trace: ToolTrace = serde_json::from_value(json!({"name":"x","arguments":{},"result":null})).unwrap();
        assert_eq!(trace.server, None);
        assert!(serde_json::to_value(trace).unwrap().get("server").is_none());
    }

    // Независимый JSON-RPC fixture: проверяем реальный HTTP-шов SDK,
    // включая initialize и две страницы каталога.
    #[tokio::test]
    async fn discovers_real_catalog_pages_over_http() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let calls = seen.clone();
        let router = Router::new().route("/mcp", post(move |Json(request): Json<Value>| {
            let calls = calls.clone();
            async move {
                let method = request["method"].as_str().unwrap().to_owned();
                calls.lock().unwrap().push(method.clone());
                let id = &request["id"];
                let result = match method.as_str() {
                    "server/discover" => return Json(json!({"jsonrpc":"2.0", "id":id,
                        "error":{"code":-32601,"message":"Method not found"}})).into_response(),
                    "initialize" => json!({"protocolVersion":"2025-11-25",
                        "capabilities":{"tools":{}}, "serverInfo":{"name":"fixture","version":"1"}}),
                    "notifications/initialized" => return StatusCode::ACCEPTED.into_response(),
                    "tools/list" if request["params"]["cursor"] == "page-2" => json!({
                        "tools":[{"name":"read_note","description":"Read a note",
                        "inputSchema":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}}]}),
                    "tools/list" => json!({"nextCursor":"page-2", "tools":[{
                        "name":"find_notes","description":"Find notes",
                        "inputSchema":{"type":"object","properties":{}}}]}),
                    _ => panic!("unexpected MCP method: {method}"),
                };
                Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let result = discover(&url).await;
        server.abort();
        let catalog = result.unwrap();
        assert_eq!(catalog["server"]["serverInfo"]["name"], "fixture");
        assert_eq!(catalog["tools"].as_array().unwrap().len(), 2);
        assert_eq!(catalog["tools"][0]["name"], "find_notes");
        assert_eq!(catalog["tools"][1]["inputSchema"]["required"], json!(["id"]));
        let calls = seen.lock().unwrap();
        assert!(calls.iter().any(|m| m == "notifications/initialized"));
        assert_eq!(calls.iter().filter(|m| *m == "tools/list").count(), 2);
        assert!(!calls.iter().any(|m| m == "tools/call"));
    }

    #[tokio::test]
    async fn unavailable_server_is_an_error_not_an_empty_catalog() {
        let router = Router::new().route("/mcp", post(|| async { StatusCode::SERVICE_UNAVAILABLE }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let result = discover(&url).await;
        server.abort();
        assert!(result.unwrap_err().contains("MCP: не удалось подключиться"));
    }
}
