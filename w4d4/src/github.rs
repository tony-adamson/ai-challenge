//! Публичный GitHub Search API: инструмент `search_repositories` и снимки
//! для наблюдений. MCP-сервер, который их отдаёт, — в `watch.rs`.
use std::time::Duration;

use rmcp::model::Tool;
use serde::Deserialize;
use serde_json::{json, Value};

pub const TOOL: &str = "search_repositories";
const API_URL: &str = "https://api.github.com/search/repositories";
const MAX_BODY: usize = 128 * 1024;
/// Снимок наблюдения — 30 репозиториев; полный JSON поиска GitHub на 30
/// элементов больше 128 КиБ, поэтому у планировщика свой потолок.
pub const SNAPSHOT_LIMIT: u8 = 30;
const SNAPSHOT_MAX_BODY: usize = 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Search {
    query: String,
    limit: u8,
}

impl Search {
    pub fn parse(value: Value) -> Result<Self, String> {
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
        Self::with_url(API_URL)
    }

    pub fn with_url(api_url: &str) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .user_agent("ai-challenge-w4d4/0.1 (https://github.com/tony-adamson/ai-challenge)")
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build().map_err(|e| e.to_string())?;
        Ok(Self { client, api_url: api_url.into() })
    }

    /// Ответ инструмента: не больше 5 репозиториев, потолок тела 128 КиБ.
    pub async fn search(&self, input: Search) -> Result<Value, String> {
        self.fetch(input.query.trim(), input.limit, MAX_BODY).await
    }

    /// Снимок для наблюдения: 30 репозиториев, потолок тела 1 МиБ.
    pub async fn snapshot(&self, query: &str) -> Result<Value, String> {
        self.fetch(query.trim(), SNAPSHOT_LIMIT, SNAPSHOT_MAX_BODY).await
    }

    async fn fetch(&self, query: &str, limit: u8, max_body: usize) -> Result<Value, String> {
        let mut response = self.client.get(&self.api_url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .query(&[("q", query.to_string()), ("per_page", limit.to_string())])
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
            if bytes.len() + chunk.len() > max_body {
                return Err(format!("Ответ GitHub превышает {} КиБ", max_body / 1024));
            }
            bytes.extend_from_slice(&chunk);
        }
        let body: Value = serde_json::from_slice(&bytes).map_err(|_| "GitHub вернул некорректный JSON")?;
        let items = body["items"].as_array().ok_or("В ответе GitHub нет списка items")?;
        let repositories: Vec<Value> = items.iter().take(limit as usize).map(|repo| json!({
            "name":repo["full_name"], "url":repo["html_url"],
            "description":repo["description"].as_str().map(|s| s.chars().take(500).collect::<String>()),
            "language":repo["language"], "stars":repo["stargazers_count"],
            "pushed_at":repo["pushed_at"], "archived":repo["archived"]
        })).collect();
        Ok(json!({"query":query, "total_count":body["total_count"],
            "incomplete_results":body["incomplete_results"], "repositories":repositories}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Router};

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
            let github = Github::with_url(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
            let http = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let result = github.search(Search { query:"Rust".into(), limit:1 }).await;
            http.abort();
            assert!(result.unwrap_err().contains(expected));
        }
    }
}
