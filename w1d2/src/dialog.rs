//! Диалоговый режим: условие завершения задаёт не длина ответа, а сама модель.
//!
//! Пока данных не хватает, она возвращает `{"status": "asking", ...}` и цикл
//! продолжается; как только данных достаточно — `{"status": "done", ...}`,
//! и цикл выходит. Строгий формат здесь не украшение: на поле `status`
//! держится управление потоком.

use std::error::Error;

use serde_json::Value;

use crate::format::{self, Constraints};
use crate::llm::{self, Provider, Request};
use crate::ui;

/// Предохранитель от зацикливания, если модель никогда не скажет "done".
const MAX_TURNS: usize = 10;

pub fn run(
    client: &reqwest::blocking::Client,
    provider: &Provider,
    api_key: &str,
    model: &str,
    constraints: &Constraints,
    question: &str,
) -> Result<(), Box<dyn Error>> {
    let system_prompt = format::build_dialog_prompt(constraints);
    ui::note("system-промпт, который уходит в API:");
    ui::quote(&system_prompt);
    println!();

    let mut messages = vec![llm::system(&system_prompt), llm::user(question)];

    for turn in 1..=MAX_TURNS {
        let request = Request {
            model,
            messages: messages.clone(),
            max_tokens: constraints.max_tokens,
            stop: constraints.stop_marker.as_deref(),
            json_mode: constraints.json_mode,
        };

        let answer = ui::with_spinner(|| llm::ask(client, provider, api_key, &request))?;
        let payload = format::extract_payload(&answer.content, constraints.stop_marker.as_deref());

        if payload.is_empty() {
            ui::explain_empty(
                &answer.finish_reason,
                answer.reasoning_tokens,
                constraints.max_tokens,
            );
            ui::warn(&format!("ход {turn}: отвечать нечем — диалог прерван"));
            return Ok(());
        }

        let parsed: Value = match serde_json::from_str(&payload) {
            Ok(value) => value,
            Err(err) => {
                ui::warn(&format!(
                    "ход {turn}: ответ не разобрался как JSON ({err}), диалог прерван"
                ));
                println!("{payload}");
                return Ok(());
            }
        };

        match parsed["status"].as_str() {
            Some("asking") => {
                let text = parsed["question"].as_str().unwrap_or("(вопрос пустой)");
                println!("\n{} {}", ui::paint("модель:", ui::CYAN), text);

                let Some(answer) = crate::wizard::ask_line("ты")? else {
                    ui::note("диалог прерван");
                    return Ok(());
                };
                if answer.trim().is_empty() {
                    ui::warn("пустой ответ — диалог прерван");
                    return Ok(());
                }

                messages.push(llm::assistant(&payload));
                messages.push(llm::user(answer.trim()));
            }
            Some("done") => {
                ui::header(&format!("условие завершения сработало на ходу {turn}"));
                let result = &parsed["result"];
                ui::quote(&serde_json::to_string_pretty(result)?);

                let problems = format::compare_to_fields(result, &constraints.fields);
                if problems.is_empty() {
                    ui::ok("в result ровно заказанные поля");
                } else {
                    ui::warn("result разошёлся с заказанными полями:");
                    for problem in problems {
                        println!("   {problem}");
                    }
                }
                return Ok(());
            }
            other => {
                ui::warn(&format!(
                    "неожиданный status: {:?}, диалог прерван",
                    other.unwrap_or("<нет поля>")
                ));
                println!("{payload}");
                return Ok(());
            }
        }
    }

    ui::warn(&format!(
        "модель не завершила сбор данных за {MAX_TURNS} ходов — выходим по предохранителю"
    ));
    Ok(())
}
