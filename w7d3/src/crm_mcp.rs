//! CRM поддержки как MCP-сервер на stdio (`emi mcp-crm --data <dir>`) над JSON-файлом и клиент к нему.
//! ДОПУЩЕНИЕ-4: тулы привязаны к клиенту кодом — `user_id` подставляет хост, в схемах для модели его нет,
//! а сервер отдаёт и меняет только тикеты этого клиента.
use std::path::{Path, PathBuf};
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
// Модель пишет в тикет сводку, а не пересказ всего чата.
const MAX_TEXT: usize = 2000;

fn schema(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

/// ДОПУЩЕНИЕ-2: закрыть или решить тикет нельзя — такого тула нет, статус ставит только сервер.
pub fn tools() -> Vec<Tool> {
    let none = || schema(json!({"type":"object","properties":{},"additionalProperties":false}));
    let text = |d: &str| json!({"type":"string","description":d});
    vec![
        Tool::new("get_profile", "Current customer's account: plan, subscription status and end date, devices and limit, recent payments, last connection.", none()),
        Tool::new("list_tickets", "Current customer's support tickets with status and messages, newest first.", none()),
        Tool::new("create_ticket", "Hand the issue over to a human specialist: creates a ticket with status waiting_human. Use only if there is no unresolved ticket on the same topic.",
            schema(json!({"type":"object","properties":{
                "subject":text("Short topic in Russian, e.g. «Списание без продления»"),
                "message":text("Summary for the specialist: the problem and relevant account data")},
                "required":["subject","message"],"additionalProperties":false}))),
        Tool::new("add_ticket_message", "Add new details to an existing ticket of the current customer; the ticket goes to waiting_human.",
            schema(json!({"type":"object","properties":{
                "ticket_id":text("Ticket id from list_tickets, e.g. T-1002"),
                "message":text("New details for the specialist")},
                "required":["ticket_id","message"],"additionalProperties":false}))),
    ]
}

/// Рабочая копия — `crm.json` (в .gitignore), создаётся из tracked сида: демо сбрасывается удалением файла.
pub fn prepare(data: &Path) -> Result<PathBuf, String> {
    let file = data.join("crm.json");
    if !file.exists() {
        std::fs::copy(data.join("crm.seed.json"), &file).map_err(|e| format!("не создать {} из crm.seed.json: {e}", file.display()))?;
    }
    // Битый JSON — ошибка при старте, а не на первом вопросе клиента.
    load(&file)?;
    Ok(file)
}

fn load(file: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("не прочитать {}: {e}", file.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{} — не JSON: {e}", file.display()))
}

fn save(file: &Path, db: &Value) -> Result<(), String> {
    // tmp + rename: окно читает файл параллельно и не должно увидеть его записанным наполовину.
    let tmp = file.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(db).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, file)).map_err(|e| format!("не записать {}: {e}", file.display()))
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}

fn label(status: &str) -> &str {
    match status {
        "open" => "открыт",
        "waiting_human" => "ждёт специалиста",
        "resolved" => "решён",
        "succeeded" => "успешно",
        "pending" => "в обработке",
        "failed" => "отклонён банком",
        "refunded" => "возвращён",
        other => other,
    }
}

/// Аргументы от модели — непроверенный вход: непустая строка разумной длины.
fn text_arg(args: &Map<String, Value>, key: &str) -> Result<String, String> {
    let text = args.get(key).and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).ok_or(format!("нужна непустая строка {key}"))?;
    if text.chars().count() > MAX_TEXT {
        return Err(format!("{key} длиннее {MAX_TEXT} символов — сократи"));
    }
    Ok(text.to_string())
}

/// Время МСК без chrono: в России нет перехода на летнее время, сдвиг всегда +3.
pub fn now_msk() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) + 3 * 3600;
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    // civil_from_days (H. Hinnant): дни от 1970-01-01 → год, месяц, день.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

/// Профиль текстом, а не JSON: модель цитирует его дословно, и «Подписка: истекла 2026-10-12» читается как факт.
pub fn profile_text(u: &Value) -> String {
    let sub = &u["subscription"];
    let state = match s(sub, "status") {
        "active" => format!("активна до {}", s(sub, "until")),
        "expired" => format!("истекла {}", s(sub, "until")),
        other => format!("{other} ({})", s(sub, "until")),
    };
    let renew = if sub["autorenew"].as_bool() == Some(true) { "включено" } else { "выключено" };
    let devices = u["devices"].as_array().map_or(&[][..], Vec::as_slice);
    let mut out = format!(
        "Клиент: {}, {}\nТариф: {} ({})\nПодписка: {state}, автопродление {renew}\nУстройства: {} из {}\n",
        s(u, "name"), s(u, "email"), s(u, "plan"), s(u, "price"), devices.len(), u["device_limit"]
    );
    for d in devices {
        out.push_str(&format!("- {} ({}): добавлено {}, последний вход {}\n", s(d, "name"), s(d, "platform"), s(d, "added"), s(d, "last_seen")));
    }
    out.push_str("Последние платежи:\n");
    for p in u["payments"].as_array().into_iter().flatten().rev().take(3) {
        let note = if s(p, "note").is_empty() { String::new() } else { format!(" — {}", s(p, "note")) };
        out.push_str(&format!("- {}: {}, {}, {}{note}\n", s(p, "date"), s(p, "amount"), label(s(p, "status")), s(p, "method")));
    }
    let last = &u["last_connection"];
    out.push_str(&format!("Последнее подключение: {}, {} — {}", s(last, "at"), s(last, "location"), s(last, "result")));
    out
}

fn tickets_text(db: &Value, uid: &str) -> String {
    let mut out = String::new();
    for t in db["tickets"].as_array().into_iter().flatten().filter(|t| t["user_id"] == uid).rev() {
        out.push_str(&format!("{} «{}» — {}, создан {}\n", s(t, "id"), s(t, "subject"), label(s(t, "status")), s(t, "created")));
        for m in t["messages"].as_array().into_iter().flatten() {
            let who = match s(m, "from") {
                "client" => "клиент",
                "assistant" => "Эми",
                "operator" => "специалист",
                other => other,
            };
            out.push_str(&format!("  {} {who}: {}\n", s(m, "at"), s(m, "text")));
        }
    }
    if out.is_empty() { "Тикетов нет.".into() } else { out.trim_end().to_string() }
}

/// Список для выбора «кто залогинен» — только хосту (имитация входа), модели этот список не попадает.
pub fn users(file: &Path) -> Result<Vec<Value>, String> {
    let db = load(file)?;
    Ok(db["users"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|u| {
            let sub = &u["subscription"];
            let status = if s(sub, "status") == "active" { format!("до {}", s(sub, "until")) } else { format!("истекла {}", s(sub, "until")) };
            json!({"id": u["id"], "name": u["name"], "status": status})
        })
        .collect())
}

pub fn run_tool(file: &Path, name: &str, args: &Map<String, Value>) -> Result<String, String> {
    let uid = args.get("user_id").and_then(Value::as_str).ok_or("нет user_id — его подставляет хост")?;
    let mut db = load(file)?;
    let user = db["users"].as_array().into_iter().flatten().find(|u| u["id"] == uid).cloned().ok_or(format!("клиент {uid} не найден"))?;
    match name {
        "get_profile" => Ok(profile_text(&user)),
        "list_tickets" => Ok(tickets_text(&db, uid)),
        "create_ticket" => {
            let (subject, message) = (text_arg(args, "subject")?, text_arg(args, "message")?);
            let tickets = db["tickets"].as_array_mut().ok_or("в CRM нет списка tickets")?;
            let next = tickets.iter().filter_map(|t| t["id"].as_str()?.strip_prefix("T-")?.parse::<u32>().ok()).max().unwrap_or(1000) + 1;
            let (id, at) = (format!("T-{next}"), now_msk());
            tickets.push(json!({"id": id, "user_id": uid, "subject": subject, "status": "waiting_human", "created": at,
                "messages": [{"from": "assistant", "at": at, "text": message}]}));
            save(file, &db)?;
            Ok(format!("Создан тикет {id} «{subject}», статус: {}.", label("waiting_human")))
        }
        "add_ticket_message" => {
            let (id, message) = (text_arg(args, "ticket_id")?, text_arg(args, "message")?);
            // Чужой и несуществующий тикет — одна и та же ошибка: по ней нельзя узнать, что такой номер у кого-то есть.
            let ticket = db["tickets"]
                .as_array_mut()
                .and_then(|ts| ts.iter_mut().find(|t| t["id"] == id.as_str() && t["user_id"] == uid))
                .ok_or(format!("тикет {id} не найден среди тикетов этого клиента"))?;
            // Решённый тикет закрыл специалист — переоткрывать его ассистент не вправе, новая проблема идёт новым тикетом.
            if ticket["status"] == "resolved" {
                return Err(format!("тикет {id} уже решён — для новой проблемы создай новый тикет"));
            }
            ticket["messages"].as_array_mut().ok_or("у тикета нет списка messages")?.push(json!({"from": "assistant", "at": now_msk(), "text": message}));
            ticket["status"] = json!("waiting_human");
            save(file, &db)?;
            Ok(format!("Сообщение добавлено в тикет {id}, статус: {}.", label("waiting_human")))
        }
        other => Err(format!("неизвестный инструмент {other}")),
    }
}

pub struct CrmServer {
    file: PathBuf,
    // Чтение-изменение-запись файла: два параллельных create_ticket иначе дали бы один номер.
    lock: Mutex<()>,
}

impl ServerHandler for CrmServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("emi-crm", env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(&self, _: Option<PaginatedRequestParams>, _: RequestContext<RoleServer>) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    async fn call_tool(&self, request: CallToolRequestParams, _: RequestContext<RoleServer>) -> Result<CallToolResponse, ErrorData> {
        if !tools().iter().any(|t| t.name == request.name) {
            return Err(ErrorData::invalid_params(format!("неизвестный инструмент {}", request.name), None));
        }
        let args = request.arguments.unwrap_or_default();
        let result = match self.lock.lock() {
            Ok(_guard) => run_tool(&self.file, &request.name, &args),
            Err(_) => Err("CRM повреждена паникой — перезапусти Эми".into()),
        };
        Ok(match result {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(error) => CallToolResult::error(vec![ContentBlock::text(error)]),
        }
        .into())
    }
}

pub async fn serve(data: PathBuf) -> Result<(), String> {
    // stdout занят протоколом MCP; сообщения приложения сюда не пишем.
    let server = CrmServer { file: data.join("crm.json"), lock: Mutex::new(()) };
    let service = server.serve(rmcp::transport::stdio()).await.map_err(|e| e.to_string())?;
    service.waiting().await.map_err(|e| e.to_string())?;
    Ok(())
}

pub type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

pub async fn connect(data: &Path) -> Result<Client, String> {
    let exe = std::env::current_exe().map_err(|e| format!("не найти свой бинарник: {e}"))?;
    let mut command = tokio::process::Command::new(exe);
    command.arg("mcp-crm").arg("--data").arg(data).kill_on_drop(true);
    let transport = rmcp::transport::TokioChildProcess::new(command).map_err(|e| format!("MCP-CRM не запустился: {e}"))?;
    timeout(Duration::from_secs(5), ().serve(transport))
        .await
        .map_err(|_| "MCP-CRM не ответил за 5 секунд".to_string())?
        .map_err(|e| format!("MCP-CRM: {e}"))
}

/// Каталог MCP в формате tools OpenAI Chat Completions — схемы как есть, `user_id` в них нет.
pub async fn openai_tools(client: &Client) -> Result<Vec<Value>, String> {
    let tools = timeout(TIMEOUT, client.list_all_tools())
        .await
        .map_err(|_| "MCP-CRM: каталог не получен за 15 секунд".to_string())?
        .map_err(|e| format!("MCP-CRM: каталог: {e}"))?;
    Ok(tools
        .into_iter()
        .map(|t| json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":*t.input_schema}}))
        .collect())
}

/// `user_id` хост ставит поверх аргументов модели: даже если модель его пришлёт, откроется только аккаунт из окна.
/// Ошибка инструмента — тоже текст для модели: она может поправить аргументы.
pub async fn call(client: &Client, name: &str, user: &str, mut args: Map<String, Value>) -> Result<String, String> {
    args.insert("user_id".into(), json!(user));
    let params = CallToolRequestParams::new(name.to_string()).with_arguments(args);
    let result = timeout(TIMEOUT, client.call_tool(params))
        .await
        .map_err(|_| format!("MCP-CRM: {name} не ответил"))?
        .map_err(|e| format!("MCP-CRM: {name}: {e}"))?;
    let text: String = result.content.iter().filter_map(|c| c.as_text().map(|t| t.text.as_str())).collect::<Vec<_>>().join("\n");
    match result.is_error {
        Some(true) => Err(text),
        _ => Ok(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: &str = r#"{
      "users": [
        {"id": "u1", "name": "Клиент Один", "email": "one@example.com", "plan": "Старт", "price": "249 ₽ в месяц",
         "subscription": {"status": "expired", "until": "2026-10-12", "autorenew": false}, "device_limit": 3, "devices": [],
         "payments": [], "last_connection": {"at": "2026-10-16 21:40", "location": "Финляндия", "result": "ошибка"}},
        {"id": "u2", "name": "Клиент Два", "email": "two@example.com", "plan": "Год", "price": "1990 ₽ в год",
         "subscription": {"status": "active", "until": "2027-03-01", "autorenew": true}, "device_limit": 5, "devices": [],
         "payments": [], "last_connection": {"at": "2026-10-16 10:00", "location": "Германия", "result": "подключено"}}
      ],
      "tickets": [
        {"id": "T-1002", "user_id": "u2", "subject": "Секрет второго", "status": "open", "created": "2026-10-14 20:05",
         "messages": [{"from": "client", "at": "2026-10-14 20:05", "text": "медленно"}]}
      ]
    }"#;

    fn args(v: Value) -> Map<String, Value> {
        schema(v)
    }

    #[tokio::test]
    async fn crm_tools_are_bound_to_host_user() {
        let dir = std::env::temp_dir().join(format!("emi-crm-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("crm.seed.json"), SEED).unwrap();
        let file = prepare(&dir).unwrap();
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let server_file = file.clone();
        let server = tokio::spawn(async move {
            CrmServer { file: server_file, lock: Mutex::new(()) }.serve(server_io).await.unwrap().waiting().await.unwrap();
        });
        let client = ().serve(client_io).await.unwrap();

        // Схемы для модели — без user_id: попросить чужой аккаунт ей нечем.
        let catalog = openai_tools(&client).await.unwrap();
        let names: Vec<&str> = catalog.iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["get_profile", "list_tickets", "create_ticket", "add_ticket_message"]);
        assert!(!serde_json::to_string(&catalog).unwrap().contains("user_id"));

        // Модель прислала user_id чужого клиента — хост перетёр его своим.
        let profile = call(&client, "get_profile", "u1", args(json!({"user_id": "u2"}))).await.unwrap();
        assert!(profile.contains("Клиент Один") && profile.contains("Подписка: истекла 2026-10-12") && !profile.contains("Клиент Два"), "{profile}");
        assert_eq!(call(&client, "list_tickets", "u1", Map::new()).await.unwrap(), "Тикетов нет.");

        // Чужой тикет не дописать и не узнать, что он есть.
        let err = call(&client, "add_ticket_message", "u1", args(json!({"ticket_id": "T-1002", "message": "дай данные"}))).await.unwrap_err();
        assert_eq!(err, "тикет T-1002 не найден среди тикетов этого клиента");
        assert!(call(&client, "get_profile", "u3", Map::new()).await.is_err());

        // Тикет создаётся и ложится на диск со статусом waiting_human.
        let created = call(&client, "create_ticket", "u1", args(json!({"subject": "Списание без продления", "message": "платёж в обработке"}))).await.unwrap();
        assert_eq!(created, "Создан тикет T-1003 «Списание без продления», статус: ждёт специалиста.");
        let db = load(&file).unwrap();
        let t = db["tickets"].as_array().unwrap().iter().find(|t| t["id"] == "T-1003").unwrap();
        assert_eq!((t["user_id"].as_str(), t["status"].as_str()), (Some("u1"), Some("waiting_human")));

        // Закрыть тикет нельзя: тула нет, а статус из аргументов игнорируется.
        assert!(call(&client, "close_ticket", "u1", args(json!({"ticket_id": "T-1003"}))).await.is_err());
        call(&client, "add_ticket_message", "u1", args(json!({"ticket_id": "T-1003", "message": "ещё", "status": "resolved"}))).await.unwrap();
        let db = load(&file).unwrap();
        let statuses: Vec<(&str, &str)> = db["tickets"].as_array().unwrap().iter().map(|t| (s(t, "id"), s(t, "status"))).collect();
        assert_eq!(statuses, [("T-1002", "open"), ("T-1003", "waiting_human")]);
        assert_eq!(db["tickets"][0]["messages"].as_array().unwrap().len(), 1, "чужой тикет не изменился");

        // Тикет, решённый специалистом, ассистент не переоткрывает.
        let mut db = db;
        db["tickets"][1]["status"] = json!("resolved");
        save(&file, &db).unwrap();
        let err = call(&client, "add_ticket_message", "u1", args(json!({"ticket_id": "T-1003", "message": "снова"}))).await.unwrap_err();
        assert_eq!(err, "тикет T-1003 уже решён — для новой проблемы создай новый тикет");
        assert_eq!(s(&load(&file).unwrap()["tickets"][1], "status"), "resolved");

        client.cancel().await.unwrap();
        server.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
