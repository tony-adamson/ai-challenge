//! Второй MCP-endpoint процесса наблюдений: отправка сообщения владельцу в Telegram.
use std::sync::Arc;
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
use serde_json::{json, Value};

use crate::summary::Telegram;

#[derive(Clone)]
pub struct Notifier {
    telegram: Option<Arc<Telegram>>,
    http: reqwest::Client,
}

impl Notifier {
    pub fn new(telegram: Option<Telegram>) -> Self {
        Self {
            telegram: telegram.map(Arc::new),
            http: reqwest::Client::builder().timeout(Duration::from_secs(20)).build()
                .expect("HTTP-клиент собирается"),
        }
    }
}

impl ServerHandler for Notifier {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("telegram-notify", "0.1.0"))
    }

    async fn list_tools(&self, _: Option<PaginatedRequestParams>, _: RequestContext<RoleServer>)
        -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(vec![tool()]))
    }

    async fn call_tool(&self, request: CallToolRequestParams, _: RequestContext<RoleServer>)
        -> Result<CallToolResponse, ErrorData> {
        if request.name != "send_telegram" {
            return Err(ErrorData::invalid_params("Неизвестный инструмент", None));
        }
        let result = async {
            let text = request.arguments.as_ref().and_then(|args| args.get("text"))
                .and_then(Value::as_str).filter(|text| !text.trim().is_empty())
                .ok_or("Нужен text (непустая строка)")?;
            let telegram = self.telegram.as_ref()
                .ok_or("Telegram не настроен: нет TELEGRAM_BOT_TOKEN/TELEGRAM_CHAT_ID")?;
            telegram.send(&self.http, text).await?;
            Ok::<(), String>(())
        }.await;
        Ok(match result {
            Ok(()) => CallToolResult::structured(json!({"sent": true})),
            Err(error) => CallToolResult::structured_error(json!({"error": error})),
        }.into())
    }
}

fn tool() -> Tool {
    Tool::new("send_telegram", "Отправить сообщение владельцу в Telegram. Получатель задан на сервере.",
        Arc::new(json!({"type":"object","properties":{"text":{"type":"string","minLength":1,"maxLength":4096}},
            "required":["text"],"additionalProperties":false}).as_object().unwrap().clone()))
}

pub fn router(notifier: Notifier) -> axum::Router {
    let service = StreamableHttpService::new(move || Ok(notifier.clone()),
        Arc::new(LocalSessionManager::default()), StreamableHttpServerConfig::default());
    axum::Router::new().nest_service("/notify", service)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::Path, routing::post, Json, Router};
    use std::sync::Mutex;

    async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, server)
    }

    #[tokio::test]
    async fn notify_tools_and_delivery() {
        let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
        let calls = seen.clone();
        let fixture = Router::new().route("/{bot}/sendMessage", post(move |Path(_bot): Path<String>, Json(body): Json<Value>| {
            let calls = calls.clone();
            async move {
                calls.lock().unwrap().push(body);
                Json(json!({"ok":true}))
            }
        }));
        let (api, telegram_server) = serve(fixture).await;
        let telegram = Telegram { api, token: "t".into(), chat_id: "42".into() };
        let (url, server) = serve(router(Notifier::new(Some(telegram)))).await;
        let client = crate::mcp::connect(&format!("{url}/notify")).await.unwrap();

        let tools = client.list_all_tools().await.unwrap();
        assert_eq!(tools.iter().map(|t| t.name.as_ref()).collect::<Vec<_>>(), ["send_telegram"]);
        assert_eq!(tools[0].input_schema["properties"].as_object().unwrap().keys().collect::<Vec<_>>(), ["text"]);
        assert_eq!(crate::mcp::call(&client, "send_telegram", json!({"text":"привет"})).await.unwrap(), json!({"sent":true}));
        assert_eq!(seen.lock().unwrap().as_slice(), &[json!({"chat_id":"42","text":"привет","disable_web_page_preview":true})]);

        let error = crate::mcp::call(&client, "send_telegram", json!({"text":"   "})).await.unwrap_err();
        assert_eq!(error, "Нужен text (непустая строка)");
        assert_eq!(seen.lock().unwrap().len(), 1, "пустой текст не уходит в Telegram");
        crate::mcp::close(client).await;
        server.abort();
        telegram_server.abort();
    }

    #[tokio::test]
    async fn unconfigured_telegram_returns_error() {
        let (url, server) = serve(router(Notifier::new(None))).await;
        let client = crate::mcp::connect(&format!("{url}/notify")).await.unwrap();
        let error = crate::mcp::call(&client, "send_telegram", json!({"text":"привет"})).await.unwrap_err();
        assert!(error.contains("Telegram не настроен"), "{error}");
        crate::mcp::close(client).await;
        server.abort();
    }
}
