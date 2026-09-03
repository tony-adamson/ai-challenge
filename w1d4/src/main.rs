//! w1d4 — температура. Один запрос уходит с temperature 0 / 0.7 / 1.2
//! по N повторов, ответы стримятся в браузер тремя колонками, внизу —
//! сводка: точность по эталону, уникальные ответы, мера разнообразия.
//! Оценивает оператор; программа только раскладывает материал рядом.

use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio::task::{JoinHandle, JoinSet};
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

const ADDR: &str = "127.0.0.1:8788";
/// Отчёт лежит рядом с Cargo.toml независимо от того, откуда запущен бинарник.
const REPORT_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/REPORT.md");
/// Температуры из задания дня.
const TEMPERATURES: [f64; 3] = [0.0, 0.7, 1.2];
const MAX_TOKENS: u32 = 1500;
const TIMEOUT: Duration = Duration::from_secs(120);

/// Пресеты — по критериям задания. У задач на точность есть эталон,
/// остальное оценивает оператор глазами. Две задачи на точность потому,
/// что на простой модель не ошибается ни при каком T, а на двухшаговой
/// без рассуждения плавает даже при T=0 — обе картины нужны для вывода.
struct Preset {
    id: &'static str,
    label: &'static str,
    prompt: &'static str,
    expected: &'static str,
}

const PRESETS: [Preset; 4] = [
    Preset {
        id: "accuracy-easy",
        label: "Точность: простая арифметика",
        prompt: "Сколько будет 17 × 23? Ответь только числом.",
        expected: "391",
    },
    Preset {
        id: "accuracy-hard",
        label: "Точность: задача в два действия",
        prompt: "В классе 32 ученика. Три восьмых из них — мальчики. Сколько девочек? Ответь только числом.",
        expected: "20",
    },
    Preset {
        id: "creative",
        label: "Креативность: название и слоган",
        prompt: "Придумай название для кофейни в одном слове и слоган одной фразой. \
                 Только название и слоган, без пояснений.",
        expected: "",
    },
    Preset {
        id: "format",
        label: "Формат: строгий JSON",
        prompt: "Верни JSON-объект с полями name, price (число в рублях) и tags (массив из трёх строк) \
                 для товара «беспроводные наушники». Только JSON, без пояснений и без markdown.",
        expected: "",
    },
];

/// Два формата API: OpenAI Chat Completions (DeepSeek, OpenRouter) и
/// Anthropic Messages. Различаются заголовками, телом и SSE-событиями.
#[derive(Clone, Copy)]
enum Api {
    OpenAi,
    Anthropic,
}

#[derive(Clone)]
struct Model {
    id: String,
    label: String,
    api: Api,
    base_url: String,
    name: String,
    api_key: String,
    /// Верхняя граница temperature у провайдера: у Anthropic это 1.0,
    /// колонка 1.2 для него зажимается и помечается в интерфейсе.
    max_temperature: f64,
}

struct App {
    tx: broadcast::Sender<String>,
    /// Текущий эксперимент: живой хэндл — идёт; по нему же работает остановка.
    experiment: Mutex<Option<JoinHandle<()>>>,
    client: Client,
    models: Vec<Model>,
}

#[derive(Deserialize)]
struct RunRequest {
    prompt: String,
    #[serde(default)]
    expected: String,
    runs: u32,
    #[serde(default)]
    model: String,
}

#[tokio::main]
async fn main() {
    if let Err(err) = start().await {
        eprintln!("Ошибка: {err}");
        std::process::exit(1);
    }
}

async fn start() -> Result<(), String> {
    println!("AI Advent · w1d4 — температура");
    match dotenvy::dotenv() {
        Ok(path) => println!("  ✓ .env прочитан: {}", path.display()),
        Err(_) => println!("  · .env не найден, беру переменные окружения"),
    }
    let models = models_from_env()?;
    for model in &models {
        println!("  ✓ модель: {} (встроенное рассуждение выключено)", model.label);
    }
    if models.len() == 1 {
        println!("  · Anthropic не подключён: нет ANTHROPIC_API_KEY в .env");
    }

    let client = Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| format!("не удалось создать HTTP-клиент: {e}"))?;
    let (tx, _) = broadcast::channel(1 << 16);
    let app = Arc::new(App { tx, experiment: Mutex::new(None), client, models });

    let router = Router::new()
        .route("/", get(|| async { Html(include_str!("../static/index.html")) }))
        .route("/config", get(config))
        .route("/run", post(run))
        .route("/stop", post(stop))
        .route("/events", get(events))
        .with_state(app);

    let listener = tokio::net::TcpListener::bind(ADDR)
        .await
        .map_err(|e| format!("не удалось занять порт {ADDR}: {e}"))?;
    let url = format!("http://{ADDR}");
    println!("  ✓ дашборд: {url}\nCtrl-C — выход\n");
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(&url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(&url).spawn();

    axum::serve(listener, router)
        .await
        .map_err(|e| format!("сервер остановился: {e}"))
}

fn env_nonempty(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.trim().is_empty())
}

/// Основная модель — из `LLM_PROVIDER`/`LLM_MODEL`, как в прошлые дни.
/// Anthropic подключается вторым пунктом, если в .env есть ключ. Sonnet 5
/// для этого дня не годится: он отклоняет `temperature` с 400, поэтому
/// по умолчанию Sonnet 4.6 — последний Sonnet, принимающий температуру.
fn models_from_env() -> Result<Vec<Model>, String> {
    let provider = std::env::var("LLM_PROVIDER").unwrap_or_else(|_| "deepseek".to_string());
    let (base_url, default_model, key_var) = match provider.as_str() {
        "deepseek" => ("https://api.deepseek.com/v1", "deepseek-v4-flash", "DEEPSEEK_API_KEY"),
        "openrouter" => ("https://openrouter.ai/api/v1", "deepseek/deepseek-v4-flash", "OPENROUTER_API_KEY"),
        other => return Err(format!("неизвестный LLM_PROVIDER: {other} (ожидается deepseek или openrouter)")),
    };
    let api_key = env_nonempty(key_var)
        .ok_or_else(|| format!("не задан {key_var}: скопируй .env.example в .env и впиши ключ"))?;
    let name = env_nonempty("LLM_MODEL").unwrap_or_else(|| default_model.to_string());
    let mut models = vec![Model {
        id: "primary".to_string(),
        label: format!("{provider} · {name}"),
        api: Api::OpenAi,
        base_url: base_url.to_string(),
        name,
        api_key,
        max_temperature: 2.0,
    }];
    if let Some(api_key) = env_nonempty("ANTHROPIC_API_KEY") {
        let name = env_nonempty("ANTHROPIC_MODEL").unwrap_or_else(|| "claude-sonnet-4-6".to_string());
        models.push(Model {
            id: "anthropic".to_string(),
            label: format!("anthropic · {name}"),
            api: Api::Anthropic,
            base_url: "https://api.anthropic.com/v1".to_string(),
            name,
            api_key,
            max_temperature: 1.0,
        });
    }
    Ok(models)
}

async fn config(State(app): State<Arc<App>>) -> Json<Value> {
    let presets: Vec<Value> = PRESETS
        .iter()
        .map(|p| json!({ "id": p.id, "label": p.label, "prompt": p.prompt, "expected": p.expected }))
        .collect();
    let models: Vec<Value> = app
        .models
        .iter()
        .map(|m| json!({ "id": m.id, "label": m.label, "max_temperature": m.max_temperature }))
        .collect();
    Json(json!({ "models": models, "temperatures": TEMPERATURES, "presets": presets }))
}

async fn events(State(app): State<Arc<App>>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(app.tx.subscribe()).map(|message| {
        let data = match message {
            Ok(json) => json,
            // Клиент отстал от канала: пусть узнает о дыре, а не увидит её в тексте.
            Err(BroadcastStreamRecvError::Lagged(count)) => json!({ "type": "lagged", "count": count }).to_string(),
        };
        Ok(Event::default().data(data))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn run(State(app): State<Arc<App>>, Json(req): Json<RunRequest>) -> (StatusCode, String) {
    if !(1..=10).contains(&req.runs) {
        return (StatusCode::BAD_REQUEST, "число повторов: от 1 до 10".to_string());
    }
    if req.prompt.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "введи текст запроса".to_string());
    }
    let Some(model) = app.models.iter().find(|m| m.id == req.model).cloned() else {
        return (StatusCode::BAD_REQUEST, "выбери модель".to_string());
    };
    let mut slot = app.experiment.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_some_and(|handle| !handle.is_finished()) {
        return (StatusCode::CONFLICT, "эксперимент уже идёт".to_string());
    }
    *slot = Some(tokio::spawn(experiment(app.clone(), req, model)));
    (StatusCode::ACCEPTED, "ok".to_string())
}

async fn stop(State(app): State<Arc<App>>) -> (StatusCode, String) {
    let handle = app.experiment.lock().unwrap_or_else(|e| e.into_inner()).take();
    match handle {
        Some(handle) if !handle.is_finished() => {
            // Отмена роняет JoinSet эксперимента, а с ним и все платные вызовы.
            handle.abort();
            let _ = app.tx.send(json!({ "type": "stopped" }).to_string());
            (StatusCode::OK, "остановлено".to_string())
        }
        _ => (StatusCode::CONFLICT, "эксперимент не идёт".to_string()),
    }
}

struct CellResult {
    temperature: f64,
    run: u32,
    text: String,
    correct: Option<bool>,
    ms: u128,
    tokens: u64,
    error: Option<String>,
}

async fn experiment(app: Arc<App>, req: RunRequest, model: Model) {
    let prompt = req.prompt.trim().to_string();
    let expected = req.expected.trim().to_string();
    let _ = app.tx.send(
        json!({ "type": "run_start", "prompt": prompt, "expected": expected, "runs": req.runs, "model": model.label })
            .to_string(),
    );

    // Все температуры и повторы идут одновременно: так разброс между
    // повторами виден вживую, а не по одному ответу в минуту.
    let mut cells = JoinSet::new();
    for temperature in TEMPERATURES {
        for run in 1..=req.runs {
            let app = app.clone();
            let model = model.clone();
            let prompt = prompt.clone();
            let expected = expected.clone();
            cells.spawn(async move { ask(&app, &model, temperature, run, &prompt, &expected).await });
        }
    }
    let mut results = Vec::new();
    while let Some(joined) = cells.join_next().await {
        match joined {
            Ok(result) => results.push(result),
            Err(err) => eprintln!("ячейка упала: {err}"),
        }
    }
    results.sort_by(|a, b| a.temperature.total_cmp(&b.temperature).then(a.run.cmp(&b.run)));

    let summary = summarize(&results, !expected.is_empty());
    let _ = app.tx.send(json!({ "type": "summary", "rows": summary }).to_string());
    let finished = match write_report(&model, &prompt, &expected, &results, &summary) {
        Ok(path) => json!({ "type": "finished", "report": path }),
        Err(error) => json!({ "type": "finished", "error": error }),
    };
    let _ = app.tx.send(finished.to_string());
}

async fn ask(app: &App, model: &Model, temperature: f64, run: u32, prompt: &str, expected: &str) -> CellResult {
    let cell = format!("{temperature}-{run}");
    let emit = |value: Value| {
        let mut value = value;
        value["cell"] = json!(cell);
        let _ = app.tx.send(value.to_string());
    };
    emit(json!({ "type": "cell_start" }));
    let started = Instant::now();
    let outcome = stream(&app.client, model, temperature.min(model.max_temperature), prompt, |text| {
        emit(json!({ "type": "token", "text": text }))
    })
    .await;
    let ms = started.elapsed().as_millis();

    let mut result = CellResult { temperature, run, text: String::new(), correct: None, ms, tokens: 0, error: None };
    match outcome {
        Ok((text, tokens)) => {
            if !expected.is_empty() {
                result.correct = Some(text.to_lowercase().contains(&expected.to_lowercase()));
            }
            emit(json!({ "type": "cell_done", "ms": ms, "tokens": tokens, "correct": result.correct, "chars": text.chars().count() }));
            result.text = text;
            result.tokens = tokens;
        }
        Err(error) => {
            emit(json!({ "type": "cell_error", "error": error, "ms": ms }));
            result.error = Some(error);
        }
    }
    result
}

/// Один потоковый вызов. Возвращает текст и число токенов ответа.
/// Встроенное рассуждение выключено: с ним температура не видна —
/// при T=0 три повтора дают три разных ответа, потому что шумит само
/// рассуждение (проверено перед написанием кода). У Anthropic поле
/// `thinking` просто не передаётся: без него Sonnet 4.6 не думает.
async fn stream(
    client: &Client,
    model: &Model,
    temperature: f64,
    prompt: &str,
    mut on_token: impl FnMut(&str),
) -> Result<(String, u64), String> {
    let mut body = json!({
        "model": model.name,
        "messages": [{ "role": "user", "content": prompt }],
        "temperature": temperature,
        "max_tokens": MAX_TOKENS,
        "stream": true,
    });
    let request = match model.api {
        Api::OpenAi => {
            body["stream_options"] = json!({ "include_usage": true });
            if model.base_url.contains("openrouter") {
                body["reasoning"] = json!({ "enabled": false });
                body["usage"] = json!({ "include": true });
            } else {
                body["thinking"] = json!({ "type": "disabled" });
            }
            client.post(format!("{}/chat/completions", model.base_url)).bearer_auth(&model.api_key)
        }
        Api::Anthropic => client
            .post(format!("{}/messages", model.base_url))
            .header("x-api-key", &model.api_key)
            .header("anthropic-version", "2023-06-01"),
    };
    let mut response = request.json(&body).send().await.map_err(describe)?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(format!("API вернул {status}: {}", truncate(&text, 500)));
    }

    let mut content = String::new();
    let mut tokens = 0;
    let mut finish_reason: Option<String> = None;
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(describe)? {
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" {
                continue;
            }
            let Ok(event) = serde_json::from_str::<Value>(data) else { continue };
            if let Some(err) = event.get("error") {
                return Err(format!("API вернул ошибку в потоке: {}", truncate(&err.to_string(), 500)));
            }
            // Текст, причина остановки и usage лежат в разных местах у двух
            // форматов; всё остальное в цикле общее.
            let (text, reason, usage) = match model.api {
                Api::OpenAi => (
                    event["choices"][0]["delta"]["content"].as_str(),
                    event["choices"][0]["finish_reason"].as_str(),
                    event["usage"]["completion_tokens"].as_u64(),
                ),
                Api::Anthropic => (
                    event["delta"]["text"].as_str(),
                    event["delta"]["stop_reason"].as_str(),
                    event["usage"]["output_tokens"].as_u64(),
                ),
            };
            if let Some(reason) = reason {
                finish_reason = Some(reason.to_string());
            }
            if let Some(text) = text {
                if !text.is_empty() {
                    content.push_str(text);
                    on_token(text);
                }
            }
            if let Some(n) = usage {
                tokens = n;
            }
        }
    }
    match finish_reason.as_deref() {
        Some("stop") | Some("end_turn") | None => {}
        Some("length") | Some("max_tokens") => return Err(format!("ответ оборван лимитом {MAX_TOKENS} токенов")),
        Some(other) => return Err(format!("ответ не завершён: stop_reason={other}")),
    }
    if content.trim().is_empty() {
        return Err("модель вернула пустой ответ".to_string());
    }
    Ok((content, tokens))
}

fn describe(err: reqwest::Error) -> String {
    let cause = if err.is_timeout() {
        format!("превышено ожидание ({} с)", TIMEOUT.as_secs())
    } else if err.is_connect() {
        "не удалось соединиться с API (сеть, DNS или TLS)".to_string()
    } else {
        "сбой запроса".to_string()
    };
    format!("{cause}: {err}")
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}… (обрезано)")
}

/// Мера разнообразия: 1 − среднее попарное пересечение множеств слов
/// (Жаккар). 0 — все ответы из одних и тех же слов, 1 — общих слов нет.
/// Один ответ или меньше — сравнивать не с чем, 0.
fn diversity(texts: &[String]) -> f64 {
    let sets: Vec<HashSet<String>> = texts
        .iter()
        .map(|t| t.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(String::from).collect())
        .collect();
    let mut total = 0.0;
    let mut pairs = 0;
    for (i, a) in sets.iter().enumerate() {
        for b in &sets[i + 1..] {
            let union = a.union(b).count();
            if union > 0 {
                total += a.intersection(b).count() as f64 / union as f64;
            }
            pairs += 1;
        }
    }
    if pairs == 0 { 0.0 } else { 1.0 - total / pairs as f64 }
}

fn summarize(results: &[CellResult], has_expected: bool) -> Vec<Value> {
    TEMPERATURES
        .iter()
        .map(|&t| {
            let mine: Vec<&CellResult> = results.iter().filter(|r| r.temperature == t).collect();
            let ok: Vec<&&CellResult> = mine.iter().filter(|r| r.error.is_none()).collect();
            let texts: Vec<String> = ok.iter().map(|r| r.text.trim().to_string()).collect();
            let distinct = texts.iter().collect::<HashSet<_>>().len();
            let n = ok.len().max(1);
            json!({
                "temperature": t,
                "answers": ok.len(),
                "errors": mine.len() - ok.len(),
                "correct": has_expected.then(|| ok.iter().filter(|r| r.correct == Some(true)).count()),
                "distinct": distinct,
                "diversity": diversity(&texts),
                "avg_chars": texts.iter().map(|t| t.chars().count()).sum::<usize>() / n,
                "avg_ms": ok.iter().map(|r| r.ms).sum::<u128>() / n as u128,
                "avg_tokens": ok.iter().map(|r| r.tokens).sum::<u64>() / n as u64,
            })
        })
        .collect()
}

/// REPORT.md копится: новый прогон встаёт сверху, под заголовком файла,
/// старые остаются ниже — можно листать прошлые результаты.
fn write_report(model: &Model, prompt: &str, expected: &str, results: &[CellResult], summary: &[Value]) -> Result<String, String> {
    const TITLE: &str = "# w1d4 — температура: журнал прогонов\n";
    let mut out = format!("\n## {} · {}\n\n**Запрос:** {prompt}\n\n", utc_stamp(), model.label);
    if !expected.is_empty() {
        out.push_str(&format!("**Эталон:** {expected}\n\n"));
    }
    out.push_str("| Температура | Верно | Уникальных | Разнообразие | Длина | Время | Токены | Ошибок |\n|---|---|---|---|---|---|---|---|\n");
    for row in summary {
        let correct = row["correct"].as_u64().map(|c| format!("{c}/{}", row["answers"])).unwrap_or_else(|| "—".to_string());
        out.push_str(&format!(
            "| {} | {} | {}/{} | {:.0}% | {} симв. | {:.1} с | {} | {} |\n",
            temperature_label(row["temperature"].as_f64().unwrap_or(0.0), model), correct, row["distinct"], row["answers"],
            row["diversity"].as_f64().unwrap_or(0.0) * 100.0, row["avg_chars"],
            row["avg_ms"].as_u64().unwrap_or(0) as f64 / 1000.0, row["avg_tokens"], row["errors"],
        ));
    }
    let mut current = f64::NAN;
    for r in results {
        if r.temperature != current {
            current = r.temperature;
            out.push_str(&format!("\n**temperature = {}**\n\n", temperature_label(r.temperature, model)));
        }
        let mark = match (&r.error, r.correct) {
            (Some(_), _) => "⚠ ",
            (None, Some(true)) => "✓ ",
            (None, Some(false)) => "✗ ",
            (None, None) => "",
        };
        out.push_str(&format!("{}. {mark}{:.1} с · {} ткн\n", r.run, r.ms as f64 / 1000.0, r.tokens));
        let body = r.error.as_deref().unwrap_or(r.text.trim());
        for line in body.lines() {
            out.push_str(&format!("   > {line}\n"));
        }
    }

    // Прочитать не смогли — значит, не переписываем: иначе история прогонов
    // молча пропадёт. Отсутствие файла — единственная нормальная причина.
    let previous = match std::fs::read_to_string(REPORT_PATH) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("не удалось прочитать {REPORT_PATH}: {err}")),
    };
    let previous = previous.strip_prefix(TITLE).unwrap_or(&previous);
    // Через временный файл и rename: Ctrl-C посреди записи оставит либо
    // старый журнал, либо новый, но не усечённый.
    let tmp = format!("{REPORT_PATH}.tmp");
    std::fs::write(&tmp, format!("{TITLE}{out}{previous}"))
        .and_then(|_| std::fs::rename(&tmp, REPORT_PATH))
        .map_err(|err| format!("не удалось записать {REPORT_PATH}: {err}"))?;
    Ok(REPORT_PATH.to_string())
}

/// «1.2» или «1.2 → 1.0», если провайдер выше не пускает.
fn temperature_label(t: f64, model: &Model) -> String {
    if t > model.max_temperature {
        format!("{t} → {:.1}", model.max_temperature)
    } else {
        format!("{t}")
    }
}

/// Дата для шапки отчёта без зависимости на chrono: алгоритм Говарда Хиннанта.
fn utc_stamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    let rem = secs.rem_euclid(86_400);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02} UTC", rem / 3600, rem % 3600 / 60)
}

#[cfg(test)]
mod tests {
    use super::diversity;

    #[test]
    fn identical_answers_have_zero_diversity() {
        let texts = vec!["Кофе будит город.".to_string(), "кофе будит город".to_string()];
        assert_eq!(diversity(&texts), 0.0);
    }

    #[test]
    fn disjoint_answers_have_full_diversity() {
        let texts = vec!["один два".to_string(), "три четыре".to_string()];
        assert_eq!(diversity(&texts), 1.0);
    }

    #[test]
    fn partial_overlap_is_averaged_over_pairs() {
        // Пары: {a,b}/{a,c} → 1/3; {a,b}/{d,e} → 0; {a,c}/{d,e} → 0.
        // Среднее пересечение 1/9, разнообразие 8/9.
        let texts = vec!["a b".to_string(), "a c".to_string(), "d e".to_string()];
        assert!((diversity(&texts) - 8.0 / 9.0).abs() < 1e-9);
    }

    #[test]
    fn single_answer_is_not_diverse() {
        assert_eq!(diversity(&["одинокий".to_string()]), 0.0);
    }
}
