//! День 35: шов интеграции «токен личного кабинета → клиент» (контракт — CONTRACT.md).
//! (а) Фейковый бэкенд кабинета «Ветрило» под `/demo-api`: вход демо-аккаунтом и клиент по токену.
//! (б) Адаптер: Эми узнаёт, кто пишет, только так — HTTP-запросом к `CUSTOMER_API` с токеном из браузера.
//! ДОПУЩЕНИЕ-3: бэкенд в том же бинарнике, но Эми ходит к нему по HTTP — при переходе на настоящий меняется только URL.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::crm_mcp;

struct Demo {
    file: PathBuf,
    // ДОПУЩЕНИЕ-1: токен → id клиента только в памяти, перезапуск разлогинивает всех.
    // Один токен на демо-аккаунт: вход публичный, и поток логинов не должен ни раздувать карту, ни выбивать чужие сессии.
    sessions: Mutex<HashMap<String, String>>,
}

pub fn demo_routes(file: PathBuf) -> Router {
    let demo = Arc::new(Demo { file, sessions: Mutex::new(HashMap::new()) });
    Router::new()
        .route("/v1/accounts", get(accounts))
        .route("/v1/login", post(login))
        .route("/v1/customer", get(customer))
        .with_state(demo)
}

fn error(status: StatusCode, text: &str) -> Response {
    (status, Json(json!({"error": text}))).into_response()
}

fn users(db: &Value) -> impl Iterator<Item = &Value> {
    db["users"].as_array().into_iter().flatten()
}

pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ").map(str::trim).filter(|t| !t.is_empty())
}

/// Витрины и демо-аккаунты для страницы входа заглушки — в настоящем кабинете такого роута нет.
async fn accounts(State(demo): State<Arc<Demo>>) -> Response {
    let db = match crm_mcp::load(&demo.file) {
        Ok(db) => db,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let accounts: Vec<Value> = users(&db).map(|u| json!({"id": u["id"], "name": u["name"], "brand": u["brand"], "situation": u["demo_note"]})).collect();
    Json(json!({"brands": db["brands"], "accounts": accounts})).into_response()
}

async fn login(State(demo): State<Arc<Demo>>, Json(body): Json<Value>) -> Response {
    let id = body["account_id"].as_str().unwrap_or("");
    match crm_mcp::load(&demo.file) {
        Ok(db) if users(&db).any(|u| u["id"] == id) => {}
        Ok(_) => return error(StatusCode::NOT_FOUND, "нет такого демо-аккаунта"),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
    let Ok(mut sessions) = demo.sessions.lock() else { return error(StatusCode::INTERNAL_SERVER_ERROR, "сессии повреждены паникой") };
    if let Some(token) = sessions.iter().find(|(_, user)| *user == id).map(|(token, _)| token.clone()) {
        return Json(json!({"token": token})).into_response();
    }
    let token = match crate::random_token() {
        Ok(token) => token,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    sessions.insert(token.clone(), id.to_string());
    Json(json!({"token": token})).into_response()
}

async fn customer(State(demo): State<Arc<Demo>>, headers: HeaderMap) -> Response {
    let id = bearer(&headers).and_then(|t| demo.sessions.lock().ok()?.get(t).cloned());
    let Some(id) = id else { return error(StatusCode::UNAUTHORIZED, "unauthorized") };
    let db = match crm_mcp::load(&demo.file) {
        Ok(db) => db,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let found = users(&db).find(|u| u["id"] == id.as_str()).map(|u| contract(&db, u));
    match found {
        Some(customer) => Json(customer).into_response(),
        None => error(StatusCode::UNAUTHORIZED, "unauthorized"),
    }
}

/// Запись демо-базы → ответ по контракту. ДОПУЩЕНИЕ-7: страны серверов — свойство витрины, а не клиента.
pub fn contract(db: &Value, u: &Value) -> Value {
    let brand = db["brands"].as_array().into_iter().flatten().find(|b| b["id"] == u["brand"]).cloned().unwrap_or(json!({}));
    json!({
        "id": u["id"], "name": u["name"], "email": u["email"],
        "brand": {"id": brand["id"], "name": brand["name"]},
        "plan": u["plan"], "subscription": u["subscription"], "trial_used": u["trial_used"],
        "payments": u["payments"], "server_countries": brand["countries"],
    })
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}

/// Профиль текстом, а не JSON: модель цитирует его дословно, и «Подписка: истекла 2026-10-12» читается как факт.
pub fn profile_text(c: &Value) -> String {
    let sub = &c["subscription"];
    let state = match s(sub, "status") {
        "active" => format!("активна до {}", s(sub, "until")),
        "trial" => format!("пробный период до {}", s(sub, "until")),
        "expired" => format!("истекла {}", s(sub, "until")),
        other => format!("{other} ({})", s(sub, "until")),
    };
    let renew = if sub["autorenew"].as_bool() == Some(true) { "включён" } else { "выключен" };
    let plan = if c["plan"].is_object() { format!("{} ({})", s(&c["plan"], "name"), s(&c["plan"], "price")) } else { "не выбран".into() };
    let trial = if c["trial_used"].as_bool() == Some(true) { "уже использован" } else { "не использовался" };
    let countries: Vec<&str> = c["server_countries"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let mut out = format!(
        "Клиент: {}, {}\nСервис: {}\nТариф: {plan}\nПодписка: {state}, автоплатёж {renew}\nПробный период: {trial}\nСтраны серверов: {}\nПоследние платежи:\n",
        s(c, "name"), s(c, "email"), s(&c["brand"], "name"), countries.join(", ")
    );
    let payments: Vec<&Value> = c["payments"].as_array().into_iter().flatten().rev().take(3).collect();
    if payments.is_empty() {
        out.push_str("- платежей нет\n");
    }
    for p in payments {
        let note = if s(p, "note").is_empty() { String::new() } else { format!(" — {}", s(p, "note")) };
        out.push_str(&format!("- {}: {}, {}, {}{note}\n", s(p, "date"), s(p, "amount"), crm_mcp::label(s(p, "status")), s(p, "method")));
    }
    out.trim_end().to_string()
}

pub struct Adapter {
    url: String,
    http: reqwest::Client,
}

impl Adapter {
    pub fn new(base: &str) -> Result<Adapter, String> {
        // Напрямую, без системного прокси: у разработчика он перехватывал бы и свой localhost.
        // Таймаут короткий: кабинет отвечает из своей БД, а клиент ждёт ответа в чате.
        let http = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Adapter { url: format!("{}/v1/customer", base.trim_end_matches('/')), http })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// `Ok(None)` — бэкенд не узнал токен (401/403): клиенту «войдите заново».
    /// `Err` — бэкенд недоступен или ответил не по контракту; токен в текст ошибки не попадает.
    pub async fn customer(&self, token: &str) -> Result<Option<Value>, String> {
        let response = self.http.get(&self.url).bearer_auth(token).send().await.map_err(|e| format!("CUSTOMER_API недоступен: {e}"))?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(format!("CUSTOMER_API: HTTP {status}"));
        }
        let customer: Value = response.json().await.map_err(|e| format!("CUSTOMER_API: ответ не JSON ({e})"))?;
        // По id привязываются тикеты и лимиты: без него клиента нельзя отличить от другого.
        if customer["id"].as_str().is_none_or(|id| id.trim().is_empty()) {
            return Err("CUSTOMER_API: в ответе нет id клиента".into());
        }
        Ok(Some(customer))
    }
}

/// Демо-бэкенд на случайном порту над копией сида — общий для тестов адаптера и чата виджета.
#[cfg(test)]
pub async fn spawn_demo(name: &str) -> (String, PathBuf) {
    let dir = std::env::temp_dir().join(format!("emi-demo-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("crm.seed.json"), include_str!("../data/crm.seed.json")).unwrap();
    let file = crm_mcp::prepare(&dir).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/demo-api", listener.local_addr().unwrap());
    let routes = Router::new().nest("/demo-api", demo_routes(file.clone()));
    tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
    (base, file)
}

#[cfg(test)]
pub async fn demo_login(base: &str, account: &str) -> reqwest::Response {
    reqwest::Client::builder().no_proxy().build().unwrap().post(format!("{base}/v1/login")).json(&json!({"account_id": account})).send().await.unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn token(base: &str, account: &str) -> String {
        let v: Value = demo_login(base, account).await.json().await.unwrap();
        v["token"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn adapter_resolves_only_issued_tokens() {
        let (base, file) = spawn_demo("adapter").await;
        let adapter = Adapter::new(&base).unwrap();
        assert_eq!(adapter.customer("0000made-up-token0000").await.unwrap(), None);
        assert_eq!(demo_login(&base, "c-999").await.status().as_u16(), 404);

        let (irina, dmitry) = (token(&base, "c-101").await, token(&base, "c-201").await);
        let a = adapter.customer(&irina).await.unwrap().unwrap();
        let b = adapter.customer(&dmitry).await.unwrap().unwrap();
        // Контракт: ровно эти поля, бренд и страны — с витрины клиента.
        let mut keys: Vec<&str> = a.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["brand", "email", "id", "name", "payments", "plan", "server_countries", "subscription", "trial_used"]);
        assert_eq!((a["id"].as_str(), a["brand"]["name"].as_str(), a["subscription"]["status"].as_str()), (Some("c-101"), Some("Ветрило"), Some("expired")));
        assert!(a["server_countries"].as_array().is_some_and(|c| !c.is_empty()));
        assert_eq!((b["id"].as_str(), b["brand"]["name"].as_str()), (Some("c-201"), Some("Бриз")));
        // Повторный вход — тот же токен: поток логинов не плодит сессии и не выбивает уже вошедших.
        assert_eq!(token(&base, "c-101").await, irina);
        assert_eq!(adapter.customer(&irina).await.unwrap().unwrap()["id"], "c-101");

        // Бэкенд недоступен — ошибка, а не «клиент не найден».
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let err = Adapter::new(&dead).unwrap().customer(&irina).await.unwrap_err();
        assert!(err.starts_with("CUSTOMER_API недоступен") && !err.contains(&irina), "{err}");
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn profile_text_cites_subscription_and_countries() {
        let db: Value = serde_json::from_str(include_str!("../data/crm.seed.json")).unwrap();
        let elena = db["users"].as_array().unwrap().iter().find(|u| u["id"] == "c-202").unwrap();
        let text = profile_text(&contract(&db, elena));
        for line in ["Сервис: Бриз", "Тариф: не выбран", "Подписка: истекла 2026-10-15", "Пробный период: уже использован", "- платежей нет"] {
            assert!(text.contains(line), "{line}\n{text}");
        }
        assert!(text.contains("Страны серверов: Финляндия"), "{text}");
    }
}
