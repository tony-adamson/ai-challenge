//! Клиенты собственного MCP-сервера наблюдений (Streamable HTTP) и публичного каталога Exa.
use std::time::Duration;

use rmcp::{
    model::{ClientCapabilities, ClientConfig, Implementation, ProtocolVersion},
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
    pub arguments: Value,
    pub result: Option<Value>,
}

pub const DEFAULT_WATCH_URL: &str = "http://127.0.0.1:8800/mcp";

pub type Client = rmcp::service::RunningService<rmcp::RoleClient, ClientConfig>;

/// Адрес собственного MCP-сервера наблюдений (`--mcp-server`).
pub fn watch_url() -> String {
    std::env::var("WATCH_MCP_URL").ok().filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_WATCH_URL.into())
}

pub async fn connect(url: &str) -> Result<Client, String> {
    let transport = StreamableHttpClientTransport::from_uri(url.to_owned());
    let info = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("w4d4-researcher", env!("CARGO_PKG_VERSION")),
    );
    timeout(
        TIMEOUT,
        info.serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Auto {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                legacy_version: Some(ProtocolVersion::V_2025_11_25),
            },
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

    // Независимый JSON-RPC fixture: проверяем реальный HTTP-шов SDK,
    // включая fallback к initialize и две страницы каталога.
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
