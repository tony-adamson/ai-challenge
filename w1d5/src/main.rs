//! w1d5 — слабая / средняя / сильная модель. Один и тот же запрос уходит
//! на шесть моделей OpenRouter по N повторов, все одновременно. Ответы
//! стримятся в браузер колонками, внизу — сводка: время до первого токена,
//! полное время, токены, стоимость, скорость и доля верных ответов.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
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

mod tasks;

const ADDR: &str = "127.0.0.1:8789";
/// Отчёт лежит рядом с Cargo.toml независимо от того, откуда запущен бинарник.
const REPORT_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/REPORT.md");
const BASE_URL: &str = "https://openrouter.ai/api/v1";
/// Две тройки «слабая / средняя / сильная»: линейка одного вендора и
/// кросс-вендорная. Переопределяется переменной MODELS в .env.
const DEFAULT_MODELS: &str = "qwen/qwen3.8-flash,qwen/qwen3.8-27b,qwen/qwen3.8-max,\
                              google/gemma-3-12b-it,deepseek/deepseek-v4-flash,anthropic/claude-sonnet-5";
const TIERS: [&str; 3] = ["слабая", "средняя", "сильная"];
/// Запас на самый длинный пресет. При 1024 три модели Qwen обрывались на
/// задаче про дни между датами: они считают её вслух, по месяцам.
const MAX_TOKENS: u32 = 2048;
const TIMEOUT: Duration = Duration::from_secs(180);

/// Открытый пресет: эталона нет, качество оценивает оператор глазами —
/// на закрытых задачах разница между тройками видна цифрой, здесь текстом.
const OPEN_PROMPT: &str =
    "Объясни в пяти предложениях, почему небо голубое, для десятилетнего ребёнка.";

#[derive(Clone)]
struct Model {
    /// Идентификатор колонки: позиция в списке, а не имя модели — имена
    /// содержат «/» и попадают в ключи ячеек.
    id: String,
    /// Полное имя для API: vendor/model.
    name: String,
    /// «слабая» / «средняя» / «сильная» по позиции в тройке; если моделей
    /// не шесть, троек нет и подпись — просто номер.
    tier: String,
}

struct App {
    tx: broadcast::Sender<String>,
    /// Текущий эксперимент: живой хэндл — идёт; по нему же работает остановка.
    experiment: Mutex<Option<JoinHandle<()>>>,
    client: Client,
    models: Vec<Model>,
    api_key: String,
}

#[derive(Deserialize)]
struct RunRequest {
    prompt: String,
    #[serde(default)]
    expected: String,
    runs: u32,
}

#[derive(Deserialize)]
struct TaskQuery {
    kind: String,
}

#[tokio::main]
async fn main() {
    if let Err(err) = start().await {
        eprintln!("Ошибка: {err}");
        std::process::exit(1);
    }
}

async fn start() -> Result<(), String> {
    println!("AI Advent · w1d5 — слабая / средняя / сильная модель");
    match dotenvy::dotenv() {
        Ok(path) => println!("  ✓ .env прочитан: {}", path.display()),
        Err(_) => println!("  · .env не найден, беру переменные окружения"),
    }
    let api_key = env_nonempty("OPENROUTER_API_KEY").ok_or(
        "не задан OPENROUTER_API_KEY: скопируй .env.example в .env и впиши ключ",
    )?;
    let models = models_from_env()?;
    for model in &models {
        println!("  ✓ {} — {}", model.tier, model.name);
    }

    let client = Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| format!("не удалось создать HTTP-клиент: {e}"))?;
    let (tx, _) = broadcast::channel(1 << 16);
    let app = Arc::new(App { tx, experiment: Mutex::new(None), client, models, api_key });

    let router = Router::new()
        .route("/", get(|| async { Html(include_str!("../static/index.html")) }))
        .route("/config", get(config))
        .route("/task", get(task))
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

fn models_from_env() -> Result<Vec<Model>, String> {
    parse_models(&env_nonempty("MODELS").unwrap_or_else(|| DEFAULT_MODELS.to_string()))
}

fn parse_models(raw: &str) -> Result<Vec<Model>, String> {
    let names: Vec<&str> = raw.split(',').map(str::trim).filter(|n| !n.is_empty()).collect();
    if names.is_empty() {
        return Err("MODELS пуст: нужен список id моделей OpenRouter через запятую".to_string());
    }
    // Подписи «слабая/средняя/сильная» осмысленны только когда список —
    // две тройки из задания. Иначе честнее подписать номером.
    let triples = names.len() == 6;
    Ok(names
        .iter()
        .enumerate()
        .map(|(i, name)| Model {
            id: format!("m{i}"),
            name: name.to_string(),
            tier: if triples {
                TIERS[i % 3].to_string()
            } else {
                format!("модель {}", i + 1)
            },
        })
        .collect())
}

async fn config(State(app): State<Arc<App>>) -> Json<Value> {
    // Четыре задачи со свежими числами генерирует сервер по /task, у
    // открытого пресета текст постоянный и эталона нет.
    let mut presets: Vec<Value> = tasks::KINDS
        .iter()
        .map(|(id, label)| json!({ "id": id, "label": label, "generated": true, "prompt": "", "expected": "" }))
        .collect();
    presets.push(json!({
        "id": "open",
        "label": "Открытый: почему небо голубое",
        "generated": false,
        "prompt": OPEN_PROMPT,
        "expected": "",
    }));
    let models: Vec<Value> = app
        .models
        .iter()
        .map(|m| json!({ "id": m.id, "name": m.name, "tier": m.tier }))
        .collect();
    Json(json!({ "models": models, "presets": presets }))
}

async fn task(Query(query): Query<TaskQuery>) -> Json<Value> {
    let generated = tasks::generate(&query.kind, &mut tasks::Rng::new());
    Json(json!({ "kind": generated.kind, "prompt": generated.text, "expected": generated.expected }))
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
    let mut slot = app.experiment.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_some_and(|handle| !handle.is_finished()) {
        return (StatusCode::CONFLICT, "эксперимент уже идёт".to_string());
    }
    *slot = Some(tokio::spawn(experiment(app.clone(), req)));
    (StatusCode::ACCEPTED, "ok".to_string())
}

async fn stop(State(app): State<Arc<App>>) -> (StatusCode, String) {
    let handle = app.experiment.lock().unwrap_or_else(|e| e.into_inner()).take();
    match handle {
        Some(handle) if !handle.is_finished() => {
            // Отмена роняет JoinSet эксперимента, а с ним и все платные вызовы.
            handle.abort();
            // Дождаться фактической смерти обязательно: пока задача
            // разматывается, её ячейки шлют события в те же ключи, и
            // следующий прогон подмешал бы их к своим. Cancelled здесь —
            // штатный исход отмены, а не ошибка. Мьютекс уже отпущен:
            // std::sync::Mutex через await держать нельзя.
            let _ = handle.await;
            // «stopped» уходит последним: по нему интерфейс разблокирует
            // «Запустить», значит новый прогон начнётся уже на чистом месте.
            let _ = app.tx.send(json!({ "type": "stopped" }).to_string());
            (StatusCode::OK, "остановлено".to_string())
        }
        _ => (StatusCode::CONFLICT, "эксперимент не идёт".to_string()),
    }
}

/// Счётчики последнего чанка стрима: OpenRouter кладёт их в `usage`,
/// когда в теле запроса есть `"usage": {"include": true}`.
#[derive(Default, Clone, Copy)]
struct Usage {
    prompt_tokens: u64,
    completion_tokens: u64,
    reasoning_tokens: u64,
    /// Доллары за этот вызов, как их посчитал сам OpenRouter.
    cost: f64,
}

struct CellResult {
    model: usize,
    run: u32,
    text: String,
    correct: Option<bool>,
    /// Время до первого токена ответа; None — токенов не было.
    ttft_ms: Option<u128>,
    ms: u128,
    usage: Usage,
    /// Модель не даёт выключить рассуждение (ответила 400) — повтор ушёл
    /// без этого поля. Её время, токены и цена включают рассуждение, и
    /// сравнивать их с остальными в лоб нельзя.
    forced_reasoning: bool,
    error: Option<String>,
}

impl CellResult {
    /// Токенов ответа в секунду по полному времени вызова.
    fn tokens_per_second(&self) -> f64 {
        if self.ms == 0 {
            return 0.0;
        }
        self.usage.completion_tokens as f64 * 1000.0 / self.ms as f64
    }
}

async fn experiment(app: Arc<App>, req: RunRequest) {
    let prompt = req.prompt.trim().to_string();
    let expected = req.expected.trim().to_string();
    let _ = app.tx.send(
        json!({ "type": "run_start", "prompt": prompt, "expected": expected, "runs": req.runs }).to_string(),
    );

    // Все модели и повторы идут одновременно: сравнение по времени честно
    // только при одинаковых условиях сети и одном и том же запросе.
    let mut cells = JoinSet::new();
    for (index, model) in app.models.iter().enumerate() {
        for run in 1..=req.runs {
            let app = app.clone();
            let model = model.clone();
            let prompt = prompt.clone();
            let expected = expected.clone();
            cells.spawn(async move { ask(&app, &model, index, run, &prompt, &expected).await });
        }
    }
    let mut results = Vec::new();
    while let Some(joined) = cells.join_next().await {
        match joined {
            Ok(result) => results.push(result),
            Err(err) => eprintln!("ячейка упала: {err}"),
        }
    }
    results.sort_by(|a, b| a.model.cmp(&b.model).then(a.run.cmp(&b.run)));

    let summary = summarize(&app.models, &results, !expected.is_empty());
    let _ = app.tx.send(json!({ "type": "summary", "rows": summary }).to_string());
    let finished = match write_report(&app.models, &prompt, &expected, &results, &summary) {
        Ok(path) => json!({ "type": "finished", "report": path }),
        Err(error) => json!({ "type": "finished", "error": error }),
    };
    let _ = app.tx.send(finished.to_string());
}

async fn ask(app: &App, model: &Model, index: usize, run: u32, prompt: &str, expected: &str) -> CellResult {
    let cell = format!("{}-{run}", model.id);
    let emit = |value: Value| {
        let mut value = value;
        value["cell"] = json!(cell);
        let _ = app.tx.send(value.to_string());
    };
    emit(json!({ "type": "cell_start" }));

    let started = Instant::now();
    let mut ttft_ms = None;
    let outcome = stream(app, model, prompt, |text| {
        ttft_ms.get_or_insert_with(|| started.elapsed().as_millis());
        emit(json!({ "type": "token", "text": text }));
    })
    .await;
    let ms = started.elapsed().as_millis();

    let mut result = CellResult {
        model: index,
        run,
        text: String::new(),
        correct: None,
        ttft_ms,
        ms,
        usage: Usage::default(),
        forced_reasoning: false,
        error: None,
    };
    match outcome {
        Ok((text, usage, forced_reasoning)) => {
            if !expected.is_empty() {
                result.correct = Some(is_correct(&text, expected));
            }
            result.text = text;
            result.usage = usage;
            result.forced_reasoning = forced_reasoning;
            emit(json!({
                "type": "cell_done",
                "forced_reasoning": forced_reasoning,
                "ms": ms,
                "ttft_ms": ttft_ms,
                "prompt_tokens": usage.prompt_tokens,
                "completion_tokens": usage.completion_tokens,
                "reasoning_tokens": usage.reasoning_tokens,
                "cost": usage.cost,
                "tps": result.tokens_per_second(),
                "correct": result.correct,
            }));
        }
        Err(error) => {
            emit(json!({ "type": "cell_error", "error": error, "ms": ms }));
            result.error = Some(error);
        }
    }
    result
}

/// Эталоны генераторов — число или одно слово, и сравнивать их со всем
/// текстом ответа нельзя: в рассуждении «сумма 2 + 1 = 3» число эталона
/// встречается по дороге, а верный итог может стоять как «Ответ: **3**».
/// Поэтому сначала из ответа вытаскивается итог (последняя строка с
/// «Ответ:», иначе последняя непустая), потом сравнение из w1d3: число —
/// по значению, слово — с точностью до окончания.
///
/// Свой эталон из нескольких слов — исключение: итог из одной строки под
/// него не подходит, такой ищется подстрокой по всему ответу, как в w1d4.
fn is_correct(text: &str, expected: &str) -> bool {
    if expected.contains(char::is_whitespace) {
        text.to_lowercase().contains(&expected.to_lowercase())
    } else {
        tasks::matches(&tasks::extract_answer(text), expected)
    }
}

/// Некоторые модели (qwen3.8-max) отказываются работать с выключенным
/// рассуждением и отвечают 400. Единственный случай, когда запрос
/// повторяется: общих ретраев здесь нет, ошибка провайдера должна быть
/// видна как ошибка, а не прятаться за повтором.
fn reasoning_is_mandatory(error: &str) -> bool {
    if !error.contains("400") {
        return false;
    }
    let error = error.to_lowercase();
    error.contains("reasoning is mandatory")
        || (error.contains("reasoning") && error.contains("cannot be disabled"))
}

/// Один потоковый вызов к OpenRouter. Возвращает текст, счётчики и признак
/// «рассуждение пришлось оставить включённым».
///
/// `temperature` не отправляется вообще: Sonnet 5 отклоняет это поле с 400
/// (проверено в w1d4), а сравнивать модели можно только на одинаковых
/// условиях — значит, все идут на дефолте провайдера. Встроенное
/// рассуждение выключено (`reasoning.enabled=false`): иначе сильные модели
/// платили бы временем и токенами за режим, которого у слабых нет.
async fn stream(
    app: &App,
    model: &Model,
    prompt: &str,
    mut on_token: impl FnMut(&str),
) -> Result<(String, Usage, bool), String> {
    match attempt(app, model, prompt, true, &mut on_token).await {
        // Отказ приходит статусом 400 до первого токена, так что повтор
        // ничего не дублирует в карточке.
        Err(error) if reasoning_is_mandatory(&error) => attempt(app, model, prompt, false, &mut on_token)
            .await
            .map(|(text, usage)| (text, usage, true)),
        other => other.map(|(text, usage)| (text, usage, false)),
    }
}

async fn attempt(
    app: &App,
    model: &Model,
    prompt: &str,
    disable_reasoning: bool,
    on_token: &mut impl FnMut(&str),
) -> Result<(String, Usage), String> {
    let mut body = json!({
        "model": model.name,
        "messages": [{ "role": "user", "content": prompt }],
        "max_tokens": MAX_TOKENS,
        "stream": true,
        "usage": { "include": true },
    });
    if disable_reasoning {
        body["reasoning"] = json!({ "enabled": false });
    }
    let mut response = app
        .client
        .post(format!("{BASE_URL}/chat/completions"))
        .bearer_auth(&app.api_key)
        .json(&body)
        .send()
        .await
        .map_err(describe)?;

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(format!("API вернул {status}: {}", api_error(&text)));
    }

    let mut content = String::new();
    let mut usage = Usage::default();
    let mut finish_reason: Option<String> = None;
    let mut buf: Vec<u8> = Vec::new();
    // Ошибку провайдера OpenRouter отдаёт обычным JSON со статусом 200 и без
    // событий SSE. Начало тела сохраняется, чтобы в такой ответ было что
    // показать вместо «модель вернула пустой ответ».
    let mut head = String::new();
    let mut saw_data = false;
    while let Some(chunk) = response.chunk().await.map_err(describe)? {
        if head.len() < 4096 {
            head.push_str(&String::from_utf8_lossy(&chunk));
        }
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else { continue };
            saw_data = true;
            let data = data.trim();
            if data == "[DONE]" {
                continue;
            }
            let Ok(event) = serde_json::from_str::<Value>(data) else { continue };
            if event.get("error").is_some_and(|e| !e.is_null()) {
                return Err(format!("API вернул ошибку в потоке: {}", api_error(&event["error"].to_string())));
            }
            if let Some(reason) = event["choices"][0]["finish_reason"].as_str() {
                finish_reason = Some(reason.to_string());
            }
            if let Some(text) = event["choices"][0]["delta"]["content"].as_str() {
                if !text.is_empty() {
                    content.push_str(text);
                    on_token(text);
                }
            }
            // Счётчики приходят один раз, последним чанком.
            let u = &event["usage"];
            if u.is_object() {
                usage = Usage {
                    prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0),
                    completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
                    reasoning_tokens: u["completion_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or(0),
                    cost: u["cost"].as_f64().unwrap_or(0.0),
                };
            }
        }
    }
    if !saw_data {
        return Err(format!("вместо потока событий пришло: {}", api_error(head.trim())));
    }
    match finish_reason.as_deref() {
        Some("stop") | None => {}
        Some("length") => return Err(format!("ответ оборван лимитом {MAX_TOKENS} токенов")),
        Some(other) => return Err(format!("ответ не завершён: finish_reason={other}")),
    }
    if content.trim().is_empty() {
        return Err("модель вернула пустой ответ".to_string());
    }
    Ok((content, usage))
}

/// Из тела ошибки OpenRouter вытаскивается человеческая часть: сообщение и
/// текст провайдера. Не разобралось — показывается начало тела как есть.
fn api_error(text: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return truncate(text, 300);
    };
    let error = if value["error"].is_object() { &value["error"] } else { &value };
    let message = error["message"].as_str().unwrap_or("").trim().to_string();
    let raw = error["metadata"]["raw"].as_str().unwrap_or("").trim();
    let joined = if raw.is_empty() || message.contains(raw) {
        message
    } else {
        format!("{message} — {raw}")
    };
    if joined.is_empty() {
        truncate(text, 300)
    } else {
        truncate(&joined, 300)
    }
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

/// Всё считается по удачным повторам: у неудачного нет ни счётчиков, ни
/// стоимости — ошибка обрывает вызов до последнего чанка стрима.
/// Стоимость, в отличие от остального, суммарная, а не средняя.
///
/// Ни одного удачного повтора — метрики не ноль, а `null`: модель, которая
/// не ответила ни разу, иначе выглядела бы в сводке самой быстрой и
/// бесплатной. Интерфейс и отчёт показывают на месте `null` прочерк.
fn summarize(models: &[Model], results: &[CellResult], has_expected: bool) -> Vec<Value> {
    models
        .iter()
        .enumerate()
        .map(|(index, model)| {
            let mine: Vec<&CellResult> = results.iter().filter(|r| r.model == index).collect();
            let ok: Vec<&&CellResult> = mine.iter().filter(|r| r.error.is_none()).collect();
            let avg = |sum: f64| (!ok.is_empty()).then(|| sum / ok.len() as f64);
            let ttft: Vec<u128> = ok.iter().filter_map(|r| r.ttft_ms).collect();
            json!({
                "model": model.name,
                "tier": model.tier,
                "answers": ok.len(),
                "errors": mine.len() - ok.len(),
                "correct": (has_expected && !ok.is_empty())
                    .then(|| ok.iter().filter(|r| r.correct == Some(true)).count()),
                "avg_ttft_ms": (!ttft.is_empty()).then(|| ttft.iter().sum::<u128>() as f64 / ttft.len() as f64),
                "avg_ms": avg(ok.iter().map(|r| r.ms as f64).sum()),
                "avg_prompt_tokens": avg(ok.iter().map(|r| r.usage.prompt_tokens as f64).sum()),
                "avg_completion_tokens": avg(ok.iter().map(|r| r.usage.completion_tokens as f64).sum()),
                "reasoning_tokens": (!ok.is_empty())
                    .then(|| ok.iter().map(|r| r.usage.reasoning_tokens).sum::<u64>()),
                "avg_tps": avg(ok.iter().map(|r| r.tokens_per_second()).sum()),
                "total_cost": (!ok.is_empty()).then(|| ok.iter().map(|r| r.usage.cost).sum::<f64>()),
            })
        })
        .collect()
}

/// Метрика из сводки для таблицы: `null` (ни одного удачного повтора)
/// показывается прочерком, а не нулём.
fn dash(value: &Value, format: impl Fn(f64) -> String) -> String {
    value.as_f64().map(format).unwrap_or_else(|| "—".to_string())
}

/// REPORT.md копится: новый прогон встаёт сверху, под заголовком файла,
/// старые остаются ниже — можно листать прошлые результаты.
fn write_report(
    models: &[Model],
    prompt: &str,
    expected: &str,
    results: &[CellResult],
    summary: &[Value],
) -> Result<String, String> {
    const TITLE: &str = "# w1d5 — модели: журнал прогонов\n";
    let mut out = format!("\n## {}\n\n**Запрос:** {prompt}\n\n", tasks::utc_stamp());
    if !expected.is_empty() {
        out.push_str(&format!("**Эталон:** {expected}\n\n"));
    }
    out.push_str(
        "| Модель | Класс | Верно | TTFT | Время | Токены in/out | Рассуждение | Ткн/с | Стоимость | Ошибок |\n\
         |---|---|---|---|---|---|---|---|---|---|\n",
    );
    for row in summary {
        let correct = row["correct"]
            .as_u64()
            .map(|c| format!("{c}/{}", row["answers"]))
            .unwrap_or_else(|| "—".to_string());
        let tokens = match (row["avg_prompt_tokens"].as_f64(), row["avg_completion_tokens"].as_f64()) {
            (Some(prompt), Some(completion)) => format!("{prompt:.0}/{completion:.0}"),
            _ => "—".to_string(),
        };
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            row["model"].as_str().unwrap_or(""),
            row["tier"].as_str().unwrap_or(""),
            correct,
            dash(&row["avg_ttft_ms"], |ms| format!("{:.1} с", ms / 1000.0)),
            dash(&row["avg_ms"], |ms| format!("{:.1} с", ms / 1000.0)),
            tokens,
            dash(&row["reasoning_tokens"], |n| format!("{n:.0}")),
            dash(&row["avg_tps"], |tps| format!("{tps:.1}")),
            dash(&row["total_cost"], |cost| format!("${cost:.6}")),
            row["errors"],
        ));
    }
    let mut current = usize::MAX;
    for r in results {
        if r.model != current {
            current = r.model;
            let model = &models[current];
            out.push_str(&format!("\n**{} — {}**\n\n", model.name, model.tier));
        }
        let mark = match (&r.error, r.correct) {
            (Some(_), _) => "⚠ ",
            (None, Some(true)) => "✓ ",
            (None, Some(false)) => "✗ ",
            (None, None) => "",
        };
        out.push_str(&format!(
            "{}. {mark}{:.1} с (первый токен {}) · {} ткн · ${:.6}{}\n",
            r.run,
            r.ms as f64 / 1000.0,
            r.ttft_ms.map(|ms| format!("{:.1} с", ms as f64 / 1000.0)).unwrap_or_else(|| "—".to_string()),
            r.usage.completion_tokens,
            r.usage.cost,
            if r.forced_reasoning { " · рассуждение обязательно" } else { "" },
        ));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn six_models_get_tier_labels_by_position() {
        let models = parse_models(DEFAULT_MODELS).expect("список по умолчанию разбирается");
        assert_eq!(models.len(), 6);
        let tiers: Vec<&str> = models.iter().map(|m| m.tier.as_str()).collect();
        assert_eq!(tiers, ["слабая", "средняя", "сильная", "слабая", "средняя", "сильная"]);
        assert_eq!(models[0].id, "m0");
        // Перенос строки в константе не должен просачиваться в имя модели.
        assert_eq!(models[3].name, "google/gemma-3-12b-it");
    }

    #[test]
    fn other_lengths_get_numbered_labels() {
        let models = parse_models("a/one, b/two ,").expect("список из двух моделей разбирается");
        assert_eq!(models.len(), 2);
        assert_eq!(models[1].name, "b/two");
        assert_eq!(models[1].tier, "модель 2");
        assert!(parse_models(" , ").is_err());
    }

    #[test]
    fn numeric_expected_is_not_matched_inside_a_longer_number() {
        assert!(is_correct("Ответ: 7", "7"));
        assert!(!is_correct("Ответ: 17", "7"));
        assert!(is_correct("это была среда", "среда"));
        // Свой эталон из нескольких слов — подстрока, как в w1d4.
        assert!(is_correct("Итог: РАССЕЯНИЕ СВЕТА в атмосфере", "рассеяние света"));
        assert!(!is_correct("поглощение света", "рассеяние света"));
    }

    fn cell(model: usize, ms: u128, tokens: u64, cost: f64, error: Option<&str>) -> CellResult {
        CellResult {
            model,
            run: 1,
            text: String::new(),
            correct: Some(true),
            ttft_ms: Some(100),
            ms,
            usage: Usage { prompt_tokens: 10, completion_tokens: tokens, reasoning_tokens: 0, cost },
            forced_reasoning: false,
            error: error.map(String::from),
        }
    }

    /// Модель, не ответившая ни разу, не должна выглядеть самой быстрой,
    /// самой дешёвой и без единой ошибки в эталоне.
    #[test]
    fn a_model_without_answers_reports_no_metrics() {
        let models = parse_models("a/ok,b/dead").expect("две модели разбираются");
        let results = vec![cell(0, 2000, 50, 0.001, None), cell(1, 300, 0, 0.0, Some("API вернул 429"))];
        let rows = summarize(&models, &results, true);

        assert_eq!(rows[0]["answers"], 1);
        assert_eq!(rows[0]["avg_ms"], 2000.0);
        assert_eq!(rows[0]["avg_tps"], 25.0);
        assert_eq!(rows[0]["total_cost"], 0.001);
        assert_eq!(rows[0]["correct"], 1);

        let dead = &rows[1];
        assert_eq!(dead["answers"], 0);
        assert_eq!(dead["errors"], 1);
        for metric in ["avg_ttft_ms", "avg_ms", "avg_prompt_tokens", "avg_completion_tokens",
                       "avg_tps", "total_cost", "reasoning_tokens", "correct"] {
            assert!(dead[metric].is_null(), "{metric} у модели без ответов должен быть null");
            assert_eq!(dash(&dead[metric], |v| format!("{v:.1}")), "—");
        }
    }

    #[test]
    fn only_the_mandatory_reasoning_400_is_retried() {
        assert!(reasoning_is_mandatory(
            "API вернул 400 Bad Request: Reasoning is mandatory for this endpoint and cannot be disabled."
        ));
        assert!(reasoning_is_mandatory("API вернул 400 Bad Request: reasoning cannot be disabled"));
        // Ни другой статус, ни другая ошибка повтора не заслуживают.
        assert!(!reasoning_is_mandatory("API вернул 429 Too Many Requests: engine_overloaded"));
        assert!(!reasoning_is_mandatory("API вернул 400 Bad Request: max_tokens is too large"));
    }

    #[test]
    fn tokens_per_second_uses_full_time() {
        let cell = CellResult {
            model: 0,
            run: 1,
            text: String::new(),
            correct: None,
            ttft_ms: Some(100),
            ms: 2000,
            usage: Usage { completion_tokens: 50, ..Usage::default() },
            forced_reasoning: false,
            error: None,
        };
        assert!((cell.tokens_per_second() - 25.0).abs() < 1e-9);
    }
}
