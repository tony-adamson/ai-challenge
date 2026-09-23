//! Каталог инструментов Exa. Один запрос — одно короткое MCP-соединение.
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

pub async fn discover(url: &str) -> Result<Value, String> {
    let transport = StreamableHttpClientTransport::from_uri(url.to_owned());
    let info = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("w4d1-researcher", env!("CARGO_PKG_VERSION")),
    );
    let mut client = timeout(
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
    .map_err(|e| format!("MCP: не удалось подключиться: {e}"))?;

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
