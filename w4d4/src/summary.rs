//! Фоновый цикл сводок: раз в `SUMMARY_EVERY_MINUTES` берёт через MCP
//! `watch_list` и `watch_summary` по каждому наблюдению, просит DeepSeek
//! написать один дайджест и шлёт его в Telegram. Что и когда ушло —
//! в `data/summary_state.json`: оттуда же панель «Наблюдения» берёт последний дайджест.
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agent::Agent;
use crate::mcp;

pub const STATE_FILE: &str = "data/summary_state.json";
const TELEGRAM_API: &str = "https://api.telegram.org";
const TELEGRAM_LIMIT: usize = 4096;

#[derive(Default, Serialize, Deserialize)]
pub struct State {
    /// id наблюдения → время последнего снимка, попавшего в отправленную сводку.
    #[serde(default)]
    pub since: BTreeMap<i64, String>,
    pub digest: Option<String>,
    /// Миллисекунды Unix, как `updated_at` у чатов.
    pub sent_at: Option<u128>,
    pub status: Option<String>,
}

pub fn load(path: &str) -> State {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save(path: &str, state: &State) {
    let text = serde_json::to_string_pretty(state).expect("state сериализуется");
    if let Err(error) = std::fs::write(path, text) {
        eprintln!("сводка: {path} не сохранён: {error}");
    }
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

pub struct Telegram {
    pub api: String,
    pub token: String,
    pub chat_id: String,
}

impl Telegram {
    pub fn from_env() -> Option<Telegram> {
        let var = |name| std::env::var(name).ok().filter(|v: &String| !v.trim().is_empty());
        Some(Telegram { api: TELEGRAM_API.into(), token: var("TELEGRAM_BOT_TOKEN")?, chat_id: var("TELEGRAM_CHAT_ID")? })
    }

    /// Простой текст без parse_mode; длиннее лимита Telegram — обрезается.
    pub async fn send(&self, http: &reqwest::Client, text: &str) -> Result<(), String> {
        let text: String = if text.chars().count() > TELEGRAM_LIMIT {
            text.chars().take(TELEGRAM_LIMIT - 1).chain(std::iter::once('…')).collect()
        } else {
            text.to_string()
        };
        // В URL лежит токен бота: в тексте ошибки его быть не должно.
        let response = http.post(format!("{}/bot{}/sendMessage", self.api, self.token))
            .json(&json!({"chat_id": self.chat_id, "text": text, "disable_web_page_preview": true}))
            .send().await.map_err(|e| format!("Telegram недоступен: {}", e.without_url()))?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or_default();
        if !status.is_success() || body["ok"] != true {
            return Err(format!("Telegram вернул {status}: {}", body["description"].as_str().unwrap_or("без описания")));
        }
        Ok(())
    }
}

/// Есть ли в сводке что рассказывать. Ошибки запусков — тоже новость.
pub fn has_changes(summary: &Value) -> bool {
    let non_empty = |key: &str| summary[key].as_array().is_some_and(|a| !a.is_empty());
    non_empty("new_repos") || non_empty("star_growth") || non_empty("fresh_pushes")
        || summary["runs_error"].as_i64().unwrap_or(0) > 0
}

/// Один прогон цикла. `Ok(false)` — наблюдений нет, ничего не отправлено.
pub async fn cycle(agent: &Agent, http: &reqwest::Client, telegram: Option<&Telegram>,
    mcp_url: &str, state_path: &str) -> Result<bool, String> {
    let mut state = load(state_path);
    // Без статуса панель показывала бы прошлое «отправлено», хотя цикл падает.
    // Время — чтобы было видно, когда упало; период и прошлый дайджест не трогаем.
    let client = match mcp::connect(mcp_url).await {
        Ok(client) => client,
        Err(error) => {
            state.status = Some(format!("ошибка сбора наблюдений: {error}"));
            state.sent_at = Some(now_ms());
            save(state_path, &state);
            return Err(error);
        }
    };
    let collected = async {
        let list = mcp::call(&client, "watch_list", json!({})).await?;
        let mut summaries = Vec::new();
        for watch in list["watches"].as_array().into_iter().flatten() {
            let id = watch["id"].as_i64().ok_or("watch_list без id")?;
            let mut args = json!({"id": id});
            if let Some(since) = state.since.get(&id) {
                args["since"] = json!(since);
            }
            summaries.push(mcp::call(&client, "watch_summary", args).await?);
        }
        Ok::<_, String>(summaries)
    }.await;
    mcp::close(client).await;
    let summaries = match collected {
        Ok(summaries) => summaries,
        Err(error) => {
            state.status = Some(format!("ошибка сбора наблюдений: {error}"));
            state.sent_at = Some(now_ms());
            save(state_path, &state);
            return Err(error);
        }
    };
    if summaries.is_empty() {
        return Ok(false);
    }

    let text = if summaries.iter().any(has_changes) {
        match agent.digest(&json!(summaries)).await {
            Ok(text) => text,
            Err(error) => {
                state.status = Some(format!("ошибка LLM: {error}"));
                save(state_path, &state);
                return Err(error);
            }
        }
    } else {
        let last = summaries.iter().filter_map(|s| s["last_snapshot_at"].as_str()).max().unwrap_or("снимков пока нет");
        format!("Наблюдения: без изменений. Наблюдений: {}, последний снимок: {last}.", summaries.len())
    };
    let status = match telegram {
        Some(telegram) => match telegram.send(http, &text).await {
            Ok(()) => "отправлено в Telegram".to_string(),
            Err(error) => {
                state.status = Some(format!("ошибка отправки: {error}"));
                save(state_path, &state);
                return Err(error);
            }
        },
        None => {
            eprintln!("сводка: TELEGRAM_BOT_TOKEN или TELEGRAM_CHAT_ID не заданы — отправка пропущена");
            "Telegram не настроен, отправка пропущена".to_string()
        }
    };
    // Период сдвигается только после доставки (или когда доставлять некуда).
    for summary in &summaries {
        if let (Some(id), Some(last)) = (summary["watch_id"].as_i64(), summary["last_snapshot_at"].as_str()) {
            state.since.insert(id, last.to_string());
        }
    }
    state.digest = Some(text);
    state.sent_at = Some(now_ms());
    state.status = Some(status);
    save(state_path, &state);
    Ok(true)
}

pub fn every() -> Duration {
    let minutes = std::env::var("SUMMARY_EVERY_MINUTES").ok().and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|m| *m > 0).unwrap_or(60);
    Duration::from_secs(minutes * 60)
}

pub fn spawn(app: std::sync::Arc<crate::App>) {
    tokio::spawn(async move {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()
            .expect("HTTP-клиент собирается");
        let telegram = Telegram::from_env();
        let mut tick = tokio::time::interval(every());
        tick.tick().await; // первый тик мгновенный — первая сводка через интервал
        loop {
            tick.tick().await;
            if let Err(error) = cycle(&app.agent, &http, telegram.as_ref(), &mcp::watch_url(), STATE_FILE).await {
                eprintln!("сводка: {error}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::Path, routing::post, Json, Router};
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn telegram_gets_plain_text_clipped_to_the_limit() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let calls = seen.clone();
        let router = Router::new().route("/{bot}/sendMessage", post(move |Path(bot): Path<String>, Json(body): Json<Value>| {
            let calls = calls.clone();
            async move {
                calls.lock().unwrap().push((bot, body.clone()));
                if body["text"] == "сломай" {
                    return (axum::http::StatusCode::BAD_REQUEST,
                        Json(json!({"ok": false, "description": "Bad Request: chat not found"})));
                }
                (axum::http::StatusCode::OK, Json(json!({"ok": true, "result": {}})))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let telegram = Telegram { api, token: "123:secret".into(), chat_id: "42".into() };
        let http = reqwest::Client::new();

        telegram.send(&http, &"я".repeat(5000)).await.unwrap();
        let error = telegram.send(&http, "сломай").await.unwrap_err();
        server.abort();
        assert_eq!(error, "Telegram вернул 400 Bad Request: Bad Request: chat not found");
        assert!(!error.contains("secret"));
        let calls = seen.lock().unwrap();
        assert_eq!(calls[0].0, "bot123:secret");
        assert_eq!(calls[0].1["chat_id"], "42");
        assert_eq!(calls[0].1["text"].as_str().unwrap().chars().count(), 4096);
        assert!(calls[0].1["text"].as_str().unwrap().ends_with('…'));
        assert!(calls[0].1.get("parse_mode").is_none());
    }

    #[tokio::test]
    async fn unreachable_mcp_is_saved_as_status_without_moving_the_period() {
        let dir = std::env::temp_dir().join(format!("w4d4-summary-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("summary_state.json").to_string_lossy().into_owned();
        let before = State { since: BTreeMap::from([(1, "2026-09-01T10:00:00Z".to_string())]),
            digest: Some("прошлая сводка".into()), sent_at: Some(1), status: Some("отправлено в Telegram".into()) };
        save(&path, &before);
        // Agent нужен только как аргумент: до LLM цикл не доходит, ключ фиктивный.
        if Agent::new().is_err() {
            std::env::set_var("DEEPSEEK_API_KEY", "test");
        }
        let agent = Agent::new().unwrap();

        let error = cycle(&agent, &reqwest::Client::new(), None, "http://127.0.0.1:1/mcp", &path).await.unwrap_err();
        let after = load(&path);
        assert!(error.starts_with("MCP"), "{error}");
        assert_eq!(after.status, Some(format!("ошибка сбора наблюдений: {error}")));
        assert_eq!(after.since, before.since);
        assert_eq!(after.digest, before.digest);
        assert!(after.sent_at > before.sent_at);
    }

    #[test]
    fn changes_are_new_repos_growth_pushes_or_errors() {
        let quiet = json!({"new_repos": [], "star_growth": [], "fresh_pushes": [], "runs_error": 0});
        assert!(!has_changes(&quiet));
        for (key, value) in [("new_repos", json!([{"name":"a/a"}])), ("star_growth", json!([{"delta":1}])),
            ("fresh_pushes", json!([{"name":"a/a"}])), ("runs_error", json!(2))] {
            let mut changed = quiet.clone();
            changed[key] = value;
            assert!(has_changes(&changed), "{key}");
        }
    }
}
