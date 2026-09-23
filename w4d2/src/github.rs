//! Собственный MCP-сервер: один инструмент над публичным GitHub Search API.
use std::time::Duration;

use rmcp::{
    model::{CallToolRequestParams, CallToolResponse, CallToolResult, Implementation,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool},
    service::RequestContext,
    ErrorData, RoleServer, ServerHandler, ServiceExt,
};
use serde::Deserialize;
use serde_json::{json, Value};

pub const TOOL: &str = "search_repositories";
const API_URL: &str = "https://api.github.com/search/repositories";
const MAX_BODY: usize = 128 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    query: String,
    limit: u8,
}

impl Search {
    fn parse(value: Value) -> Result<Self, String> {
        let input: Self = serde_json::from_value(value)
            .map_err(|_| "Нужны query (строка) и limit (целое число 1–5)".to_string())?;
        if input.query.trim().is_empty() || input.query.chars().count() > 256 {
            return Err("query должен содержать от 1 до 256 символов".into());
        }
        if !(1..=5).contains(&input.limit) {
            return Err("limit должен быть от 1 до 5".into());
        }
        Ok(input)
    }
}

pub fn tool() -> Tool {
    Tool::new(TOOL,
        "Search public GitHub repositories. Use concise keywords and GitHub qualifiers, e.g. full-text search language:Rust archived:false. Returns repository metadata, not README or code. Stars are popularity, not code quality.",
        json!({"type":"object", "properties":{
            "query":{"type":"string","minLength":1,"maxLength":256,
                "description":"GitHub repository search query, including optional language: or topic: qualifiers"},
            "limit":{"type":"integer","minimum":1,"maximum":5,
                "description":"Maximum number of repositories to return (1–5)"}},
            "required":["query","limit"],"additionalProperties":false})
            .as_object().expect("literal schema is an object").clone())
}

pub struct Github {
    client: reqwest::Client,
    api_url: String,
}

impl Github {
    pub fn new() -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .user_agent("ai-challenge-w4d2/0.1 (https://github.com/tony-adamson/ai-challenge)")
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build().map_err(|e| e.to_string())?;
        Ok(Self { client, api_url: API_URL.into() })
    }

    async fn search(&self, input: Search) -> Result<Value, String> {
        let mut response = self.client.get(&self.api_url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .query(&[("q", input.query.trim().to_string()), ("per_page", input.limit.to_string())])
            .send().await.map_err(|e| format!("GitHub недоступен: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(match status.as_u16() {
                403 | 429 => "GitHub отказал в доступе или исчерпан лимит запросов. Повтори позже.".into(),
                422 => "GitHub не принял поисковый запрос. Упрости ключевые слова и фильтры.".into(),
                _ => format!("GitHub вернул HTTP {status}"),
            });
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
            if bytes.len() + chunk.len() > MAX_BODY {
                return Err("Ответ GitHub превышает 128 КиБ".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let body: Value = serde_json::from_slice(&bytes).map_err(|_| "GitHub вернул некорректный JSON")?;
        let items = body["items"].as_array().ok_or("В ответе GitHub нет списка items")?;
        let repositories: Vec<Value> = items.iter().take(input.limit as usize).map(|repo| json!({
            "name":repo["full_name"], "url":repo["html_url"],
            "description":repo["description"].as_str().map(|s| s.chars().take(500).collect::<String>()),
            "language":repo["language"], "stars":repo["stargazers_count"],
            "pushed_at":repo["pushed_at"], "archived":repo["archived"]
        })).collect();
        Ok(json!({"query":input.query, "total_count":body["total_count"],
            "incomplete_results":body["incomplete_results"], "repositories":repositories}))
    }
}

impl ServerHandler for Github {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("github-researcher", "0.1.0"))
    }

    async fn list_tools(&self, _: Option<PaginatedRequestParams>, _: RequestContext<RoleServer>)
        -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(vec![tool()]))
    }

    async fn call_tool(&self, request: CallToolRequestParams, _: RequestContext<RoleServer>)
        -> Result<CallToolResponse, ErrorData> {
        if request.name != TOOL {
            return Err(ErrorData::invalid_params("Неизвестный инструмент", None));
        }
        let result = match Search::parse(json!(request.arguments)) {
            Ok(input) => self.search(input).await,
            Err(error) => Err(error),
        };
        Ok(match result {
            Ok(value) => CallToolResult::structured(value),
            Err(error) => CallToolResult::structured_error(json!({"error":error})),
        }.into())
    }
}

pub async fn serve() -> Result<(), String> {
    // stdout занят протоколом MCP; сообщения приложения сюда не пишем.
    let service = Github::new()?.serve(rmcp::transport::stdio()).await.map_err(|e| e.to_string())?;
    service.waiting().await.map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::Query, http::HeaderMap, routing::get, Json, Router};
    use std::{collections::HashMap, sync::{Arc, atomic::{AtomicUsize, Ordering}}};

    #[tokio::test]
    async fn mcp_registers_validates_and_calls_github_api() {
        let hits = Arc::new(AtomicUsize::new(0));
        let observed = hits.clone();
        let router = Router::new().route("/search", get(move |Query(q): Query<HashMap<String,String>>, headers: HeaderMap| {
            let observed = observed.clone();
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                assert_eq!(q.get("q").unwrap(), "search language:Rust");
                assert_eq!(q.get("per_page").unwrap(), "1");
                assert!(!headers.contains_key("authorization"));
                Json(json!({"total_count":12,"incomplete_results":true,"items":[{
                    "full_name":"fixture/search", "html_url":"https://github.com/fixture/search",
                    "description":"Test fixture, not a real search result", "language":"Rust",
                    "stargazers_count":17,"pushed_at":"2026-01-01T00:00:00Z","archived":false}]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut github = Github::new().unwrap();
        github.api_url = format!("http://{}/search", listener.local_addr().unwrap());
        let http = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let (client_io, server_io) = tokio::io::duplex(8192);
        let server = tokio::spawn(async move {
            github.serve(server_io).await.unwrap().waiting().await.unwrap();
        });
        let client = ().serve(client_io).await.unwrap();
        let tools = client.list_all_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "search_repositories");
        assert_eq!(tools[0].input_schema["required"], json!(["query","limit"]));
        assert_eq!(tools[0].input_schema["properties"]["limit"]["maximum"], 5);
        for arguments in [json!({}), json!({"query":" ","limit":1}),
            json!({"query":"Rust","limit":6}), json!({"query":"Rust","limit":"3"}),
            json!({"query":"Rust","limit":1,"url":"https://example.com"})] {
            let result = client.call_tool(CallToolRequestParams::new(TOOL)
                .with_arguments(arguments.as_object().unwrap().clone())).await.unwrap();
            assert_eq!(result.is_error, Some(true));
        }
        assert_eq!(hits.load(Ordering::SeqCst), 0, "invalid arguments never reach GitHub");
        let result = client.call_tool(CallToolRequestParams::new(TOOL)
            .with_arguments(json!({"query":"search language:Rust","limit":1}).as_object().unwrap().clone()))
            .await.unwrap();
        assert_eq!(result.is_error, Some(false));
        let data = result.structured_content.unwrap();
        assert_eq!(data["repositories"][0]["name"], "fixture/search");
        assert_eq!(data["repositories"][0]["stars"], 17);
        assert_eq!(data["incomplete_results"], true);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(client.call_tool(CallToolRequestParams::new("delete_repository")).await.is_err());
        client.cancel().await.unwrap();
        server.await.unwrap();
        http.abort();
    }

    #[tokio::test]
    async fn upstream_errors_and_oversized_bodies_are_not_empty_successes() {
        for (status, body, expected) in [
            (403, "{}".to_string(), "лимит"),
            (200, "not json".to_string(), "JSON"),
            (200, "x".repeat(MAX_BODY + 1), "128 КиБ"),
        ] {
            let router = Router::new().route("/", get(move || async move {
                (axum::http::StatusCode::from_u16(status).unwrap(), body)
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut github = Github::new().unwrap();
            github.api_url = format!("http://{}/", listener.local_addr().unwrap());
            let http = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let result = github.search(Search { query:"Rust".into(), limit:1 }).await;
            http.abort();
            assert!(result.unwrap_err().contains(expected));
        }
    }
}
