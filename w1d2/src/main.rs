//! w1d2 — один и тот же запрос с разным уровнем контроля ответа.
//!
//! Меню выставляет ограничения, программа шлёт запрос дважды: без ограничений
//! и с ними, и показывает, что при повторных прогонах структура ответа
//! не меняется, а содержание — меняется.

mod dialog;
mod format;
mod llm;
mod ui;
mod wizard;

use std::env;
use std::error::Error;

use serde_json::Value;

use format::{Constraints, OutputFormat};
use llm::{Provider, Request};

struct App {
    client: reqwest::blocking::Client,
    provider: Provider,
    api_key: String,
    model: String,
    question: String,
    constraints: Constraints,
}

fn main() -> Result<(), Box<dyn Error>> {
    print_banner();

    // Инициализация быстрая, но пусть будет видно, из чего она состоит:
    // на этих же шагах вылезают все типичные ошибки настройки.
    let env_path = dotenvy::dotenv().ok();
    ui::step(&match &env_path {
        Some(path) => format!(".env прочитан: {}", path.display()),
        None => ".env не найден — беру переменные окружения".to_string(),
    });

    let provider_name = env::var("LLM_PROVIDER").unwrap_or_else(|_| "deepseek".to_string());
    let provider = llm::provider(&provider_name)?;
    ui::step(&format!("провайдер: {provider_name}"));

    let api_key = env::var(provider.key_var).map_err(|_| {
        format!(
            "не задан {}: скопируй .env.example в .env и впиши ключ",
            provider.key_var
        )
    })?;
    ui::step(&format!("ключ {} найден", provider.key_var));

    let model = env::var("LLM_MODEL").unwrap_or_else(|_| provider.default_model.to_string());
    let client = llm::build_client()?;
    ui::step(&format!("клиент готов, модель: {model}"));

    let mut app = App {
        client,
        provider,
        api_key,
        model,
        question: format::DEFAULT_QUESTION.to_string(),
        constraints: Constraints::default(),
    };

    println!();
    run_loop(&mut app)
}

fn print_banner() {
    println!(
        "\n{}",
        ui::paint("AI Advent · w1d2 — контроль формата ответа", ui::BOLD)
    );
    ui::note("Ctrl-C — выход в любой момент");
    println!();
}

/// Настроить → отправить → решить, что дальше.
fn run_loop(app: &mut App) -> Result<(), Box<dyn Error>> {
    let mut first = true;
    loop {
        if !first {
            ui::clear_screen();
            print_banner();
        }
        first = false;

        let Some((question, constraints)) = wizard::configure(&app.question, &app.constraints)?
        else {
            println!();
            return Ok(());
        };
        app.question = question;
        app.constraints = constraints;

        loop {
            if let Err(err) = send(app) {
                ui::fail(&format!("{err}"));
            }

            match wizard::next_action()? {
                wizard::Next::Again => continue,
                wizard::Next::Reconfigure => break,
                wizard::Next::Quit => {
                    println!();
                    return Ok(());
                }
            }
        }
    }
}

fn send(app: &App) -> Result<(), Box<dyn Error>> {
    if app.constraints.dialog {
        ui::header("диалоговый режим: модель сама решает, когда данных достаточно");
        return dialog::run(
            &app.client,
            &app.provider,
            &app.api_key,
            &app.model,
            &app.constraints,
            &app.question,
        );
    }

    run_baseline(app)?;
    run_constrained(app)
}

/// Базовая линия: тот же вопрос без единого ограничения.
fn run_baseline(app: &App) -> Result<(), Box<dyn Error>> {
    ui::header("БЕЗ ОГРАНИЧЕНИЙ");
    ui::note("system-промпт: нет · max_tokens: нет (потолок провайдера) · stop: нет · response_format: нет");

    let request = Request {
        model: &app.model,
        messages: vec![llm::user(&app.question)],
        max_tokens: None,
        stop: None,
        json_mode: false,
    };

    let answer = ui::with_spinner(|| llm::ask(&app.client, &app.provider, &app.api_key, &request))?;
    ui::quote(&answer.content);
    println!();
    ui::note(&format!(
        "длина: {} символов · finish_reason: {}",
        answer.content.chars().count(),
        answer.finish_reason
    ));
    match serde_json::from_str::<Value>(answer.content.trim()) {
        Ok(_) => ui::ok("разобрался как JSON (модель угадала сама)"),
        Err(_) => ui::warn("как JSON не разбирается — структуру машиной не взять"),
    }
    Ok(())
}

/// Тот же вопрос с выставленными ограничениями, повторённый `runs` раз.
fn run_constrained(app: &App) -> Result<(), Box<dyn Error>> {
    let c = &app.constraints;
    ui::header("С ОГРАНИЧЕНИЯМИ");

    let system_prompt = format::build_system_prompt(c);
    match &system_prompt {
        Some(prompt) => {
            ui::note("system-промпт, который уходит в API:");
            ui::quote(prompt);
        }
        None => ui::note("system-промпт: нет"),
    }
    ui::note(&format!(
        "max_tokens: {} · stop: {} · response_format: {}",
        c.max_tokens
            .map(|n| n.to_string())
            .unwrap_or_else(|| "нет".to_string()),
        c.stop_marker
            .clone()
            .map(|m| format!("[\"{m}\"]"))
            .unwrap_or_else(|| "нет".to_string()),
        if c.json_mode { "json_object" } else { "нет" }
    ));

    let mut messages = Vec::new();
    if let Some(prompt) = &system_prompt {
        messages.push(llm::system(prompt));
    }
    messages.push(llm::user(&app.question));

    // None — ответ вообще не разобрался: такие прогоны нельзя сравнивать
    // между собой, иначе три одинаковых провала выглядят как стабильный формат.
    let mut fingerprints: Vec<Option<String>> = Vec::new();
    let mut payloads: Vec<String> = Vec::new();

    for run in 1..=c.runs {
        let request = Request {
            model: &app.model,
            messages: messages.clone(),
            max_tokens: c.max_tokens,
            stop: c.stop_marker.as_deref(),
            json_mode: c.json_mode,
        };

        let answer = ui::with_spinner(|| llm::ask(&app.client, &app.provider, &app.api_key, &request))?;
        let payload = format::extract_payload(&answer.content, c.stop_marker.as_deref());

        // На двадцати прогонах полные ответы превращают экран в простыню:
        // первый показываем целиком, остальные — строкой с образцом данных.
        let verbose = c.runs <= 5 || run == 1;

        let fingerprint = if verbose {
            println!("\n{}", ui::paint(&format!("прогон {run}/{}", c.runs), ui::BOLD));
            ui::quote(&payload);
            ui::note(&format!(
                "finish_reason: {}{}",
                answer.finish_reason,
                if answer.reasoning_tokens > 0 {
                    format!(" · токенов на рассуждение: {}", answer.reasoning_tokens)
                } else {
                    String::new()
                }
            ));
            if payload.is_empty() {
                ui::explain_empty(&answer.finish_reason, answer.reasoning_tokens, c.max_tokens);
                None
            } else {
                check_run(&payload, c)
            }
        } else {
            let fingerprint = if payload.is_empty() {
                None
            } else {
                quiet_check_run(&payload, c)
            };
            print_compact(run, c.runs, &payload, fingerprint.is_some(), c.fields.first());
            fingerprint
        };
        let empty_by_limit = payload.is_empty() && answer.finish_reason == "length";
        fingerprints.push(fingerprint);
        payloads.push(payload);

        // Лимит не зависит от прогона: если он съел ответ один раз, съест и
        // остальные. Гонять платные запросы впустую незачем.
        if empty_by_limit && run < c.runs {
            ui::warn(&format!(
                "остальные {} прогонов отменены: с этим лимитом результат не изменится",
                c.runs - run
            ));
            break;
        }
    }

    verdict(&fingerprints, &payloads, c.format);
    Ok(())
}

/// Строка про один прогон в компактном режиме: статус и образец данных,
/// чтобы было видно — формат тот же, а содержание каждый раз новое.
fn print_compact(run: u32, total: u32, payload: &str, ok: bool, first_field: Option<&String>) {
    // Показываем поле, которое пользователь заказал первым, а не первое по
    // алфавиту: так в столбце видно ровно ту величину, за которой он следит.
    let sample = serde_json::from_str::<Value>(payload)
        .ok()
        .and_then(|value| {
            let map = value.as_object()?;
            let (key, val) = first_field
                .and_then(|name| map.get_key_value(name))
                .or_else(|| map.iter().next())?;
            let text = val
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| val.to_string());
            Some(format!("{key}: {}", shorten(&text, 48)))
        })
        .unwrap_or_else(|| "—".to_string());

    println!(
        "  {:>3}/{total}  {}  {}",
        run,
        if ok {
            ui::paint("✓", ui::GREEN)
        } else {
            ui::paint("✗", ui::RED)
        },
        ui::paint(&sample, ui::DIM)
    );
}

fn shorten(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    format!("{}…", text.chars().take(limit).collect::<String>())
}

/// То же, что check_run, но молча: в компактном режиме печатает вызывающий.
fn quiet_check_run(payload: &str, c: &Constraints) -> Option<String> {
    match c.format {
        OutputFormat::Json => serde_json::from_str::<Value>(payload).ok().and_then(|value| {
            format::compare_to_fields(&value, &c.fields)
                .is_empty()
                .then(|| format::structure_fingerprint(&value))
        }),
        OutputFormat::Markdown => format::markdown_fingerprint(payload)
            .filter(|columns| *columns == c.fields.join(" | ")),
        OutputFormat::Free => None,
    }
}

/// Проверяет один ответ и возвращает отпечаток его структуры.
/// `None` — ответ не разобрался в заданном формате.
fn check_run(payload: &str, c: &Constraints) -> Option<String> {
    match c.format {
        OutputFormat::Json => match serde_json::from_str::<Value>(payload) {
            Ok(value) => {
                let problems = format::compare_to_fields(&value, &c.fields);
                if problems.is_empty() {
                    ui::ok("поля ровно те, что заказаны");
                } else {
                    ui::warn("расхождения с заказанными полями:");
                    for problem in &problems {
                        println!("   {problem}");
                    }
                }
                Some(format::structure_fingerprint(&value))
            }
            Err(err) => {
                ui::fail(&format!("не разобрался как JSON: {err}"));
                None
            }
        },
        OutputFormat::Markdown => match format::markdown_fingerprint(payload) {
            Some(columns) => {
                let expected = c.fields.join(" | ");
                if columns == expected {
                    ui::ok("колонки ровно те, что заказаны");
                } else {
                    ui::warn(&format!("колонки разошлись: ожидались {expected}"));
                }
                Some(columns)
            }
            None => {
                ui::fail("таблицы нет или строки разной ширины");
                None
            }
        },
        OutputFormat::Free => {
            ui::note("формат не задан — проверять нечего");
            None
        }
    }
}

/// Главный вывод дня: структура одна, содержание разное.
fn verdict(fingerprints: &[Option<String>], payloads: &[String], format: OutputFormat) {
    ui::header("ИТОГ");

    if format == OutputFormat::Free {
        ui::note("формат не задавался — сравнивать структуру не с чем");
    } else {
        let parsed: Vec<&String> = fingerprints.iter().flatten().collect();
        let failed = fingerprints.len() - parsed.len();

        if failed > 0 {
            ui::fail(&format!(
                "{failed} из {} прогонов не дали разбираемого ответа",
                fingerprints.len()
            ));
        }

        match parsed.len() {
            0 => ui::fail("сравнивать нечего: ни один прогон не разобрался"),
            1 if fingerprints.len() == 1 => {
                ui::note("прогон один: стабильность формата на нём не проверяется")
            }
            1 => ui::note("разобрался только один прогон: стабильность не проверяется"),
            _ if parsed.windows(2).all(|pair| pair[0] == pair[1]) => ui::ok(&format!(
                "структура одинакова во всех {} разобранных прогонах",
                parsed.len()
            )),
            _ => ui::fail("структура разошлась между прогонами"),
        }
    }

    // Про содержание есть что сказать только когда структура вообще
    // подтвердилась: на провалившихся прогонах сравнивать нечего.
    let parsed_count = fingerprints.iter().flatten().count();
    if payloads.len() > 1 && parsed_count > 1 {
        let same_text = payloads.windows(2).all(|pair| pair[0] == pair[1]);
        if same_text {
            ui::note("содержание совпало дословно — модель повторилась");
        } else {
            ui::ok("содержание при этом разное — фиксируется формат, а не текст");
        }
    }
}
