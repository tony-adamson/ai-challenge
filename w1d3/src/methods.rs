//! Четыре способа получить ответ на одну и ту же задачу. Хвост с форматом
//! итога у всех одинаковый: варьируется только способ рассуждения.

use reqwest::Client;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::llm::{self, Answer, Model};

#[derive(Clone, Copy, PartialEq)]
pub enum Method {
    Direct,
    Cot,
    Meta,
    Experts,
}

impl Method {
    pub const ALL: [Method; 4] = [Method::Direct, Method::Cot, Method::Meta, Method::Experts];

    pub fn id(self) -> &'static str {
        match self {
            Method::Direct => "direct",
            Method::Cot => "cot",
            Method::Meta => "meta",
            Method::Experts => "experts",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Method::Direct => "Прямой ответ",
            Method::Cot => "Решай пошагово",
            Method::Meta => "Мета-промпт",
            Method::Experts => "Группа экспертов",
        }
    }
}

pub const FORMAT_TAIL: &str =
    "\n\nСамой последней строкой напиши итог в виде «ОТВЕТ: <значение>» — только значение, без пояснений.";

const META_REQUEST: &str = "Составь промпт — инструкцию для ИИ-ассистента, — который поможет решить \
приведённую ниже задачу максимально точно. Опиши, как подойти к решению, на что обратить внимание, \
как проверить результат. Напиши только текст промпта, саму задачу не решай.\n\nЗадача:\n";

const EXPERTS: [(&str, &str); 3] = [
    ("Аналитик", "Ты — аналитик. Разбираешь условие задачи: что дано, что спрашивается, где скрытые \
ловушки и неоднозначности. Решаешь задачу, опираясь на строгое прочтение условия."),
    ("Инженер", "Ты — инженер. Решаешь задачу точным вычислением: расписываешь каждый шаг, \
перепроверяешь арифметику и промежуточные результаты. Не доверяешь прикидкам."),
    ("Критик", "Ты — критик. Сначала называешь самый очевидный ответ, а потом ищешь, почему он \
может быть неверным: крайние случаи, ошибка на единицу, неверная интерпретация. Даёшь свой ответ \
после этой проверки."),
];

const MODERATOR: &str = "Ты — модератор консилиума. Тебе дана задача и решения трёх независимых \
экспертов. Сравни их, найди расхождения и ошибки, при необходимости пересчитай спорное место сам \
и определи верный итог.";

/// Одна карточка на экране: пара (модель, способ) в конкретном прогоне.
/// Все события карточки уходят в общий канал, из которого их читает SSE.
#[derive(Clone)]
pub struct Cell {
    pub tx: broadcast::Sender<String>,
    pub id: String,
    pub run: u32,
}

impl Cell {
    pub fn emit(&self, mut event: Value) {
        event["cell"] = json!(self.id);
        event["run"] = json!(self.run);
        // Единственная причина ошибки — нет подписчиков; терять нечего.
        let _ = self.tx.send(event.to_string());
    }

    fn section(&self, name: &str) {
        self.emit(json!({ "type": "section", "name": name }));
    }

    fn token(&self, section: &str, text: &str, thinking: bool) {
        self.emit(json!({ "type": "token", "section": section, "text": text, "thinking": thinking }));
    }
}

pub struct Outcome {
    /// Секции карточки в порядке появления: (название, текст).
    pub sections: Vec<(String, String)>,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub calls: u32,
}

impl Outcome {
    /// Итоговый ответ способа — текст последней секции.
    pub fn final_content(&self) -> &str {
        self.sections.last().map(|(_, text)| text.as_str()).unwrap_or("")
    }

    fn add(&mut self, name: &str, answer: Answer) {
        self.sections.push((name.to_string(), answer.content));
        self.completion_tokens += answer.completion_tokens;
        self.reasoning_tokens += answer.reasoning_tokens;
        self.calls += 1;
    }
}

async fn call(
    client: &Client,
    model: &Model,
    cell: &Cell,
    section: &str,
    messages: Vec<Value>,
) -> Result<Answer, String> {
    cell.section(section);
    llm::stream(client, model, messages, false, |text, thinking| cell.token(section, text, thinking)).await
}

pub async fn run(
    method: Method,
    client: &Client,
    model: &Model,
    task: &str,
    cell: &Cell,
) -> Result<Outcome, String> {
    let mut outcome = Outcome { sections: Vec::new(), completion_tokens: 0, reasoning_tokens: 0, calls: 0 };
    let task_with_tail = format!("{task}{FORMAT_TAIL}");

    match method {
        Method::Direct => {
            let answer = call(client, model, cell, "Ответ", vec![llm::user(&task_with_tail)]).await?;
            outcome.add("Ответ", answer);
        }
        Method::Cot => {
            let prompt = format!(
                "Решай пошагово: распиши рассуждение по шагам, проверь каждый шаг и только потом дай итог.\n\n{task_with_tail}"
            );
            let answer = call(client, model, cell, "Пошаговое решение", vec![llm::user(&prompt)]).await?;
            outcome.add("Пошаговое решение", answer);
        }
        Method::Meta => {
            let request = format!("{META_REQUEST}{task}");
            let prompt = call(client, model, cell, "Промпт, который модель написала себе", vec![llm::user(&request)]).await?;
            let messages = vec![llm::system(&prompt.content), llm::user(&task_with_tail)];
            outcome.add("Промпт, который модель написала себе", prompt);
            let answer = call(client, model, cell, "Решение по этому промпту", messages).await?;
            outcome.add("Решение по этому промпту", answer);
        }
        Method::Experts => {
            let expert = |(name, role): (&'static str, &'static str)| {
                let messages = vec![llm::system(role), llm::user(&task_with_tail)];
                async move { call(client, model, cell, name, messages).await.map(|a| (name, a)) }
            };
            // Эксперты не видят друг друга: три независимых контекста, а не
            // одна модель, играющая три роли в одном ответе.
            let (a, b, c) = tokio::join!(expert(EXPERTS[0]), expert(EXPERTS[1]), expert(EXPERTS[2]));
            let mut opinions = String::new();
            for (name, answer) in [a?, b?, c?] {
                opinions.push_str(&format!("\n\n### {name}\n{}", answer.content));
                outcome.add(name, answer);
            }
            let request = format!("Задача:\n{task}\n\nРешения экспертов:{opinions}\n\nОпредели верный итог.{FORMAT_TAIL}");
            let verdict = call(client, model, cell, "Модератор", vec![llm::system(MODERATOR), llm::user(&request)]).await?;
            outcome.add("Модератор", verdict);
        }
    }
    Ok(outcome)
}

const JUDGE: &str = "Ты — проверяющий. Тебе дана задача и итоговый ответ на неё. Реши задачу сам, \
сравни с ответом и вынеси вердикт. Ответь строго JSON-объектом вида \
{\"verdict\": \"correct\" | \"incorrect\" | \"unknown\", \"reason\": \"кратко почему\"}.";

/// Судья для задач без эталона: слепая оценка, судья видит только задачу и
/// итоговый ответ, не зная, каким способом тот получен. Возвращает None,
/// если судья не смог решить.
pub async fn judge(client: &Client, model: &Model, task: &str, answer: &str) -> Option<bool> {
    let request = format!("Задача:\n{task}\n\nПроверяемый итоговый ответ: {answer}\n\nВерни JSON.");
    let messages = vec![llm::system(JUDGE), llm::user(&request)];
    let reply = llm::stream(client, model, messages, true, |_, _| {}).await.ok()?;
    let parsed: Value = serde_json::from_str(&reply.content).ok()?;
    match parsed["verdict"].as_str()? {
        "correct" => Some(true),
        "incorrect" => Some(false),
        _ => None,
    }
}
