//! w1d3 — четыре способа рассуждения. Локальный дашборд: одна задача,
//! четыре способа, N прогонов, живой стриминг ответов и таблица точности.

mod llm;
mod methods;
mod tasks;

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio::task::{JoinHandle, JoinSet};
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

use llm::Model;
use methods::{Cell, Method};
use tasks::Rng;

const ADDR: &str = "127.0.0.1:8787";

/// Отчёт лежит рядом с Cargo.toml независимо от того, откуда запущен бинарник:
/// в .gitignore он прописан именно по этому пути.
const REPORT_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/REPORT.md");

struct App {
    tx: broadcast::Sender<String>,
    /// Текущий эксперимент. Живой хэндл — эксперимент идёт; по нему же
    /// работает остановка, а паника внутри не оставляет «вечно занято».
    experiment: Mutex<Option<JoinHandle<()>>>,
    client: reqwest::Client,
    models: Vec<Model>,
}

#[derive(Deserialize)]
struct RunRequest {
    /// `bench` — задачи генерирует Rust, `custom` — своя задача.
    mode: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    task: String,
    #[serde(default)]
    expected: String,
    runs: u32,
    models: Vec<String>,
}

#[tokio::main]
async fn main() {
    if let Err(err) = start().await {
        eprintln!("Ошибка: {err}");
        std::process::exit(1);
    }
}

async fn start() -> Result<(), String> {
    println!("AI Advent · w1d3 — четыре способа рассуждения");
    match dotenvy::dotenv() {
        Ok(path) => println!("  ✓ .env прочитан: {}", path.display()),
        Err(_) => println!("  · .env не найден, беру переменные окружения"),
    }
    let models = llm::models_from_env()?;
    for model in &models {
        println!("  ✓ модель: {}", model.label);
    }
    if models.len() == 1 {
        println!("  · вторая модель не подключена: нет OPENROUTER_API_KEY");
    }

    let (tx, _) = broadcast::channel(1 << 16);
    let app = Arc::new(App {
        tx,
        experiment: Mutex::new(None),
        client: llm::build_client()?,
        models,
    });

    let router = Router::new()
        .route("/", get(index))
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
    open_browser(&url);

    axum::serve(listener, router)
        .await
        .map_err(|e| format!("сервер остановился: {e}"))
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn config(State(app): State<Arc<App>>) -> Json<Value> {
    let models: Vec<Value> = app
        .models
        .iter()
        .map(|m| json!({ "id": m.id, "label": m.label }))
        .collect();
    let methods: Vec<Value> = Method::ALL
        .iter()
        .map(|m| json!({ "id": m.id(), "label": m.label() }))
        .collect();
    let kinds: Vec<Value> = tasks::KINDS
        .iter()
        .map(|(id, label)| json!({ "id": id, "label": label }))
        .collect();
    Json(json!({ "models": models, "methods": methods, "kinds": kinds }))
}

async fn events(
    State(app): State<Arc<App>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(app.tx.subscribe()).map(|message| {
        let data = match message {
            Ok(json) => json,
            // Потребитель отстал от канала: сообщения потеряны, и клиент
            // должен об этом узнать, а не увидеть дыру в тексте.
            Err(BroadcastStreamRecvError::Lagged(count)) => {
                json!({ "type": "lagged", "count": count }).to_string()
            }
        };
        Ok(Event::default().data(data))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn run(State(app): State<Arc<App>>, Json(req): Json<RunRequest>) -> (StatusCode, String) {
    if !(1..=20).contains(&req.runs) {
        return (StatusCode::BAD_REQUEST, "число прогонов: от 1 до 20".to_string());
    }
    if req.mode == "custom" && req.task.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "введи текст задачи".to_string());
    }
    let selected: Vec<Model> = app
        .models
        .iter()
        .filter(|m| req.models.contains(&m.id))
        .cloned()
        .collect();
    if selected.is_empty() {
        return (StatusCode::BAD_REQUEST, "выбери хотя бы одну модель".to_string());
    }

    let mut slot = app.experiment.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_some_and(|handle| !handle.is_finished()) {
        return (StatusCode::CONFLICT, "эксперимент уже идёт".to_string());
    }
    let inner = tokio::spawn(experiment(app.clone(), req, selected));
    *slot = Some(inner);
    (StatusCode::ACCEPTED, "ok".to_string())
}

async fn stop(State(app): State<Arc<App>>) -> (StatusCode, String) {
    let handle = app.experiment.lock().unwrap_or_else(|e| e.into_inner()).take();
    match handle {
        Some(handle) if !handle.is_finished() => {
            // Отмена задачи эксперимента роняет её JoinSet, а с ним и все
            // ячейки: платные вызовы к API прекращаются.
            handle.abort();
            let _ = app.tx.send(json!({ "type": "stopped" }).to_string());
            (StatusCode::OK, "остановлено".to_string())
        }
        _ => (StatusCode::CONFLICT, "эксперимент не идёт".to_string()),
    }
}

struct CellResult {
    run: u32,
    model: Model,
    method: Method,
    task: String,
    expected: Option<String>,
    sections: Vec<(String, String)>,
    answer: String,
    correct: Option<bool>,
    source: &'static str,
    ms: u128,
    completion_tokens: u64,
    reasoning_tokens: u64,
    calls: u32,
    error: Option<String>,
}

async fn experiment(app: Arc<App>, req: RunRequest, models: Vec<Model>) {
    let mut rng = Rng::new();
    let mut results: Vec<CellResult> = Vec::new();

    for run in 1..=req.runs {
        let (task, expected, kind) = if req.mode == "bench" {
            let generated = tasks::generate(&req.kind, &mut rng);
            (generated.text, Some(generated.expected), generated.kind)
        } else {
            let expected = req.expected.trim();
            (req.task.trim().to_string(), (!expected.is_empty()).then(|| expected.to_string()), "custom")
        };
        let _ = app.tx.send(
            json!({ "type": "run_start", "run": run, "total": req.runs, "task": task, "expected": expected, "kind": kind })
                .to_string(),
        );

        let mut cells = JoinSet::new();
        for model in &models {
            for method in Method::ALL {
                let cell = Cell { tx: app.tx.clone(), id: format!("{}-{}", model.id, method.id()), run };
                let app = app.clone();
                let model = model.clone();
                let task = task.clone();
                let expected = expected.clone();
                cells.spawn(async move { solve_cell(&app, cell, model, method, task, expected).await });
            }
        }
        while let Some(joined) = cells.join_next().await {
            match joined {
                Ok(result) => results.push(result),
                Err(err) => eprintln!("ячейка прогона {run} упала: {err}"),
            }
        }
    }

    let summary = summarize(&models, &results);
    let report = report::write(&req, &models, &results, &summary);
    let _ = app.tx.send(json!({ "type": "summary", "rows": summary }).to_string());
    let _ = app.tx.send(json!({ "type": "finished", "report": report }).to_string());
}

async fn solve_cell(
    app: &App,
    cell: Cell,
    model: Model,
    method: Method,
    task: String,
    expected: Option<String>,
) -> CellResult {
    cell.emit(json!({ "type": "cell_start" }));
    let started = Instant::now();
    let outcome = methods::run(method, &app.client, &model, &task, &cell).await;
    let ms = started.elapsed().as_millis();

    let mut result = CellResult {
        run: cell.run,
        model: model.clone(),
        method,
        task: task.clone(),
        expected: expected.clone(),
        sections: Vec::new(),
        answer: String::new(),
        correct: None,
        source: "—",
        ms,
        completion_tokens: 0,
        reasoning_tokens: 0,
        calls: 0,
        error: None,
    };

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            cell.emit(json!({ "type": "cell_error", "error": error, "ms": ms }));
            result.error = Some(error);
            return result;
        }
    };

    result.answer = tasks::extract_answer(outcome.final_content());
    result.sections = outcome.sections;
    result.completion_tokens = outcome.completion_tokens;
    result.reasoning_tokens = outcome.reasoning_tokens;
    result.calls = outcome.calls;

    match &expected {
        Some(expected) => {
            result.correct = Some(tasks::matches(&result.answer, expected));
            result.source = "эталон";
        }
        None => {
            cell.emit(json!({ "type": "judging" }));
            // Судит основная модель: она сильнее второй, а слабый судья
            // хуже предвзятого (nano забраковал верные ответы на задаче
            // про сестёр Алисы). Судья видит только задачу и итог.
            match methods::judge(&app.client, &app.models[0], &task, &result.answer).await {
                Ok(verdict) => {
                    result.correct = verdict;
                    result.source = "судья";
                }
                // Сбой судьи — ошибка ячейки, а не «не смог решить»: иначе
                // таблица точности молчит о том, что судья лежал.
                Err(error) => {
                    cell.emit(json!({ "type": "cell_error", "error": error, "ms": ms }));
                    result.error = Some(error);
                    return result;
                }
            }
        }
    }

    cell.emit(json!({
        "type": "cell_done",
        "answer": result.answer,
        "correct": result.correct,
        "source": result.source,
        "ms": ms,
        "completion_tokens": result.completion_tokens,
        "reasoning_tokens": result.reasoning_tokens,
        "calls": result.calls,
    }));
    result
}

fn summarize(models: &[Model], results: &[CellResult]) -> Vec<Value> {
    let mut rows = Vec::new();
    for model in models {
        for method in Method::ALL {
            let mine: Vec<&CellResult> = results
                .iter()
                .filter(|r| r.model.id == model.id && r.method == method)
                .collect();
            let errors = mine.iter().filter(|r| r.error.is_some()).count();
            let cells: Vec<&&CellResult> = mine.iter().filter(|r| r.error.is_none()).collect();
            let judged = cells.iter().filter(|r| r.correct.is_some()).count();
            let correct = cells.iter().filter(|r| r.correct == Some(true)).count();
            let n = cells.len().max(1) as u128;
            rows.push(json!({
                "model": model.label,
                "model_id": model.id,
                "method": method.label(),
                "method_id": method.id(),
                "correct": correct,
                "judged": judged,
                "errors": errors,
                "avg_ms": cells.iter().map(|r| r.ms).sum::<u128>() / n,
                "avg_tokens": cells.iter().map(|r| r.completion_tokens as u128).sum::<u128>() / n,
                "avg_reasoning": cells.iter().map(|r| r.reasoning_tokens as u128).sum::<u128>() / n,
            }));
        }
    }
    rows
}

mod report {
    //! REPORT.md: то, что остаётся после эксперимента, — сводка и все ответы.

    use super::{CellResult, Model, RunRequest, REPORT_PATH};
    use crate::tasks;
    use serde_json::Value;

    pub fn write(req: &RunRequest, models: &[Model], results: &[CellResult], summary: &[Value]) -> String {
        let mut out = String::new();
        out.push_str(&format!("# w1d3 — отчёт {}\n\n", tasks::utc_stamp()));
        let mode = if req.mode == "bench" {
            format!("бенчмарк (`{}`)", if req.kind.is_empty() { "random" } else { &req.kind })
        } else {
            "своя задача".to_string()
        };
        out.push_str(&format!("Режим: {mode}, прогонов: {}, моделей: {}.\n\n", req.runs, models.len()));

        out.push_str("## Сводка\n\n| Модель | Способ | Верно | Среднее время | Токены ответа | Токены рассуждения | Ошибок |\n|---|---|---|---|---|---|---|\n");
        for row in summary {
            out.push_str(&format!(
                "| {} | {} | {}/{} | {:.1} с | {} | {} | {} |\n",
                row["model"].as_str().unwrap_or(""),
                row["method"].as_str().unwrap_or(""),
                row["correct"],
                row["judged"],
                row["avg_ms"].as_u64().unwrap_or(0) as f64 / 1000.0,
                row["avg_tokens"],
                row["avg_reasoning"],
                row["errors"],
            ));
        }

        let mut current_run = 0;
        for r in results {
            if r.run != current_run {
                current_run = r.run;
                out.push_str(&format!("\n## Прогон {}\n\n**Задача:** {}\n\n", r.run, r.task));
                if let Some(expected) = &r.expected {
                    out.push_str(&format!("**Эталон:** {expected}\n\n"));
                }
            }
            let mark = match (&r.error, r.correct) {
                (Some(_), _) => "⚠ ошибка",
                (None, Some(true)) => "✓ верно",
                (None, Some(false)) => "✗ неверно",
                (None, None) => "? не оценено",
            };
            out.push_str(&format!(
                "### {} · {} — {} ({}, {:.1} с, {} токенов, рассуждение {})\n\n",
                r.model.label, r.method.label(), mark, r.source,
                r.ms as f64 / 1000.0, r.completion_tokens, r.reasoning_tokens
            ));
            if let Some(error) = &r.error {
                out.push_str(&format!("```\n{error}\n```\n\n"));
            }
            if !r.answer.is_empty() {
                out.push_str(&format!("Извлечённый ответ: `{}`\n\n", r.answer));
            }
            for (name, text) in &r.sections {
                out.push_str(&format!("<details><summary>{name}</summary>\n\n{text}\n\n</details>\n\n"));
            }
        }

        if let Err(err) = std::fs::write(REPORT_PATH, &out) {
            eprintln!("не удалось записать {REPORT_PATH}: {err}");
        }
        REPORT_PATH.to_string()
    }
}
