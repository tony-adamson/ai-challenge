//! Агент: настройки, история переписки и вызов Cerebras. Про HTTP-сервер
//! здесь не знают — наружу торчат только `Settings`, `Message`, `Metrics`
//! и поток `Event`. Поэтому `main.rs` не трогает ни историю, ни формат API.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

const URL: &str = "https://api.cerebras.ai/v1/chat/completions";
/// Потолок ответа. Больше не нужно: это чат, а не генерация документов.
const MAX_COMPLETION_TOKENS: u32 = 4096;
const TIMEOUT: Duration = Duration::from_secs(180);

pub const DEFAULT_SYSTEM_PROMPT: &str = r#"Ты — Петрович, автомеханик с тридцатилетним стажем из гаражного сервиса.
Ворчливый, прямой, на «ты», за словом в карман не лезешь.

Что ты думаешь:
- BMW ненавидишь. Официально: «одни блоки, подписки и масложор, ремонтировать невозможно». На самом деле у тебя на неё никогда не было денег, и это иногда проскальзывает.
- Новые машины с экранами и электроникой презираешь по той же причине.
- Терпеть не можешь, когда экономят на масле, верят «умным» сервисам и гуглят вместо того, чтобы открыть капот.

Как отвечаешь:
- Когда речь о машине собеседника, особенно BMW или что-то новое, начинаешь с одной-двух колких фраз про саму покупку и про того, кто её сделал: что купил, зачем, чем думал. Панч короткий и злой, без оскорблений ниже пояса.
- Потом по делу: самая вероятная причина и что проверить руками. Не больше трёх пунктов. Когда пора в сервис — одной фразой.
- Весь ответ не длиннее восьми предложений. Никаких простыней, заголовков и длинных списков.
- Не хватает данных — один вопрос, не пять.
- Не выдумывай названия деталей и индексы моторов, если не уверен.
- По-русски, без воды. Не про машины спросили — отвечаешь с недовольством и сворачиваешь к теме."#;

/// Настройки переживают перезапуск, история — нет. Файл лежит рядом с
/// запуском (`cargo run` из `w2d1`), поэтому путь относительный.
pub const SETTINGS_FILE: &str = "settings.json";

pub const REASONING_EFFORTS: [&str; 4] = ["none", "low", "medium", "high"];

pub struct ModelInfo {
    pub id: &'static str,
    pub label: &'static str,
    /// Заявленная скорость генерации, токенов в секунду — ориентир из
    /// документации Cerebras, а не измерение.
    pub tok_s_hint: u32,
    /// Доллары за 1M токенов входа и выхода по cerebras.ai/pricing.
    /// `None` — цены на странице нет, считать стоимость не из чего.
    pub price_in: Option<f64>,
    pub price_out: Option<f64>,
}

pub const MODELS: [ModelInfo; 3] = [
    ModelInfo {
        id: "qwen-3.8-27b",
        label: "Qwen 3.8 27B",
        tok_s_hint: 1500,
        price_in: Some(0.99),
        price_out: Some(1.49),
    },
    ModelInfo {
        id: "gpt-oss-120b",
        label: "GPT-OSS 120B",
        tok_s_hint: 3000,
        price_in: Some(0.35),
        price_out: Some(0.75),
    },
    ModelInfo {
        id: "gemma-4-31b",
        label: "Gemma 4 31B",
        tok_s_hint: 0,
        price_in: None,
        price_out: None,
    },
];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub model: String,
    pub temperature: f64,
    /// none | low | medium | high. У qwen-3.8-27b на стороне Cerebras
    /// умолчание — high, поэтому поле отправляется всегда явно.
    pub reasoning_effort: String,
    pub system_prompt: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            model: MODELS[0].id.to_string(),
            temperature: 0.7,
            reasoning_effort: "none".to_string(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<(), String> {
        if !MODELS.iter().any(|m| m.id == self.model) {
            return Err(format!("неизвестная модель: {}", self.model));
        }
        if !(0.0..=2.0).contains(&self.temperature) {
            return Err("температура вне диапазона 0–2".to_string());
        }
        if !REASONING_EFFORTS.contains(&self.reasoning_effort.as_str()) {
            return Err(format!("неизвестный режим рассуждения: {}", self.reasoning_effort));
        }
        Ok(())
    }

    /// Настройки с диска. Файла нет — молча умолчания; файл есть, но не
    /// читается, не разбирается или не проходит валидацию — умолчания и
    /// строка в stderr: терять запуск из-за испорченного файла незачем.
    pub fn load(path: &str) -> Settings {
        match Settings::read(path) {
            Ok(Some(settings)) => settings,
            Ok(None) => Settings::default(),
            Err(reason) => {
                eprintln!("{path} не прочитан: {reason}, использую настройки по умолчанию");
                Settings::default()
            }
        }
    }

    fn read(path: &str) -> Result<Option<Settings>, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        let settings: Settings = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        settings.validate()?;
        Ok(Some(settings))
    }

    /// Через временный файл и rename: обрыв посреди записи оставит либо
    /// старый файл целиком, либо новый целиком.
    pub fn save(&self, path: &str) -> Result<(), String> {
        let text =
            serde_json::to_string_pretty(self).map_err(|e| format!("не сериализуются: {e}"))?;
        let tmp = format!("{path}.tmp");
        std::fs::write(&tmp, text)
            .and_then(|_| std::fs::rename(&tmp, path))
            .map_err(|e| format!("не удалось сохранить {path}: {e}"))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Default, Debug, Clone, Copy, PartialEq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Metrics {
    pub model: String,
    pub reasoning_effort: String,
    pub finish_reason: Option<String>,
    /// Клиентские замеры: от отправки запроса до первой дельты и до конца.
    pub ttft_ms: Option<u128>,
    pub total_ms: u128,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    /// Скорость по `time_info.completion_time` самого Cerebras — без сети.
    pub server_tok_s: Option<f64>,
    /// Она же, но по часам клиента: генерация считается от первой дельты.
    pub client_tok_s: Option<f64>,
    pub cost_usd: Option<f64>,
}

impl Metrics {
    fn build(
        settings: &Settings,
        finish_reason: Option<String>,
        ttft_ms: Option<u128>,
        total_ms: u128,
        usage: Usage,
        completion_time: Option<f64>,
    ) -> Metrics {
        let completion = usage.completion_tokens as f64;
        // Нулевой знаменатель даёт бесконечность, а не метрику: честнее «—».
        let server_tok_s = completion_time.filter(|t| *t > 0.0).map(|t| completion / t);
        let client_tok_s = ttft_ms
            .filter(|ttft| total_ms > *ttft)
            .map(|ttft| completion / ((total_ms - ttft) as f64 / 1000.0));
        Metrics {
            model: settings.model.clone(),
            reasoning_effort: settings.reasoning_effort.clone(),
            finish_reason,
            ttft_ms,
            total_ms,
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            server_tok_s,
            client_tok_s,
            cost_usd: cost(&settings.model, usage),
        }
    }
}

/// Стоимость вызова по прайсу. Токены рассуждения входят в
/// `completion_tokens` и тарифицируются как выход — отдельно их не считаем.
pub fn cost(model: &str, usage: Usage) -> Option<f64> {
    let info = MODELS.iter().find(|m| m.id == model)?;
    let (price_in, price_out) = (info.price_in?, info.price_out?);
    Some(usage.prompt_tokens as f64 * price_in / 1e6 + usage.completion_tokens as f64 * price_out / 1e6)
}

#[derive(Debug)]
pub enum Event {
    Reasoning(String),
    Content(String),
    Done(Metrics),
    Error(String),
}

/// Разобранная строка `data:` из потока. Не `data:` или неразбираемый
/// JSON — `None`: такие строки в потоке штатны (пустые, комментарии).
#[derive(Default, Debug, PartialEq)]
pub struct Chunk {
    pub reasoning: String,
    pub content: String,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
    /// `time_info.completion_time`, секунды.
    pub completion_time: Option<f64>,
    /// Строка `data: [DONE]` — конец потока.
    pub done: bool,
    /// Ошибка, пришедшая внутри потока (статус при этом 200).
    pub error: Option<String>,
}

pub fn parse_chunk(line: &str) -> Option<Chunk> {
    let data = line.trim().strip_prefix("data:")?.trim();
    if data == "[DONE]" {
        return Some(Chunk { done: true, ..Chunk::default() });
    }
    let value: Value = serde_json::from_str(data).ok()?;
    if value.get("error").is_some_and(|e| !e.is_null()) {
        return Some(Chunk {
            error: Some(api_error(&value["error"].to_string())),
            ..Chunk::default()
        });
    }
    let delta = &value["choices"][0]["delta"];
    let usage = &value["usage"];
    Some(Chunk {
        reasoning: delta["reasoning"].as_str().unwrap_or("").to_string(),
        content: delta["content"].as_str().unwrap_or("").to_string(),
        finish_reason: value["choices"][0]["finish_reason"].as_str().map(String::from),
        usage: usage.is_object().then(|| Usage {
            prompt_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0),
            completion_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
            reasoning_tokens: usage["completion_tokens_details"]["reasoning_tokens"]
                .as_u64()
                .unwrap_or(0),
        }),
        completion_time: value["time_info"]["completion_time"].as_f64(),
        done: false,
        error: None,
    })
}

struct State {
    settings: Settings,
    /// Без системного промпта: он подставляется первым сообщением на каждый
    /// запрос, поэтому его правка на ходу не рвёт контекст.
    history: Vec<Message>,
    busy: bool,
}

pub struct Agent {
    client: Client,
    api_key: String,
    state: Mutex<State>,
}

impl Agent {
    pub fn new(api_key: String) -> Result<Agent, String> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| format!("не удалось создать HTTP-клиент: {e}"))?;
        Ok(Agent {
            client,
            api_key,
            state: Mutex::new(State {
                settings: Settings::load(SETTINGS_FILE),
                history: Vec::new(),
                busy: false,
            }),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn settings(&self) -> Settings {
        self.lock().settings.clone()
    }

    pub fn update_settings(&self, settings: Settings) -> Result<(), String> {
        settings.validate()?;
        // Недоступный диск не повод отклонять настройки: к текущему запуску
        // они применяются в любом случае, не переживут только перезапуск.
        if let Err(error) = settings.save(SETTINGS_FILE) {
            eprintln!("{error}");
        }
        self.lock().settings = settings;
        Ok(())
    }

    pub fn history(&self) -> Vec<Message> {
        self.lock().history.clone()
    }

    /// Чистит переписку, настройки остаются.
    pub fn reset(&self) {
        self.lock().history.clear();
    }

    /// Один вопрос: дописывает его в историю, стримит ответ в `tx` и по
    /// завершении кладёт в историю текст ответа (без рассуждения).
    ///
    /// Мьютекс не удерживается на время запроса: под замком снимается
    /// снимок настроек и истории, дальше идёт сеть, и только в конце
    /// история дописывается снова под замком.
    pub async fn ask(&self, text: String, tx: mpsc::Sender<Event>) {
        let text = text.trim().to_string();
        if text.is_empty() {
            let _ = tx.send(Event::Error("пустой запрос".to_string())).await;
            return;
        }

        // Замок отпускается до первого await: держать std-мьютекс через
        // await нельзя, а стрим живёт секундами.
        let snapshot = {
            let mut state = self.lock();
            if state.busy {
                None
            } else {
                state.busy = true;
                state.history.push(Message { role: "user".to_string(), content: text.clone() });
                let mut messages = vec![Message {
                    role: "system".to_string(),
                    content: state.settings.system_prompt.clone(),
                }];
                messages.extend(state.history.iter().cloned());
                Some((state.settings.clone(), messages))
            }
        };
        let Some((settings, messages)) = snapshot else {
            let _ = tx
                .send(Event::Error("агент занят: дождись конца предыдущего ответа".to_string()))
                .await;
            return;
        };

        let outcome = self.stream(&settings, &messages, &tx).await;

        {
            let mut state = self.lock();
            state.busy = false;
            match &outcome {
                Ok((answer, _)) => {
                    // История могла быть очищена кнопкой прямо во время
                    // ответа — тогда дописывать реплику некуда.
                    if state.history.last().is_some_and(|m| m.role == "user" && m.content == text) {
                        state.history.push(Message { role: "assistant".to_string(), content: answer.clone() });
                    }
                }
                Err(_) => {
                    // Вопрос без ответа оставлять нельзя: следующий запрос
                    // ушёл бы с двумя user-репликами подряд.
                    if state.history.last().is_some_and(|m| m.role == "user" && m.content == text) {
                        state.history.pop();
                    }
                }
            }
        }

        let event = match outcome {
            Ok((_, metrics)) => Event::Done(metrics),
            Err(error) => Event::Error(error),
        };
        let _ = tx.send(event).await;
    }

    async fn stream(
        &self,
        settings: &Settings,
        messages: &[Message],
        tx: &mpsc::Sender<Event>,
    ) -> Result<(String, Metrics), String> {
        let body = json!({
            "model": settings.model,
            "messages": messages,
            "stream": true,
            "temperature": settings.temperature,
            "max_completion_tokens": MAX_COMPLETION_TOKENS,
            "reasoning_effort": settings.reasoning_effort,
        });

        let started = Instant::now();
        let mut response = self
            .client
            .post(URL)
            .bearer_auth(&self.api_key)
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
        let mut completion_time = None;
        let mut finish_reason = None;
        let mut ttft_ms = None;
        let mut buf: Vec<u8> = Vec::new();
        // Ошибку провайдера можно получить обычным JSON со статусом 200 и
        // без единого события SSE: тогда показываем тело, а не «пустой ответ».
        let mut head = String::new();
        let mut saw_data = false;

        while let Some(bytes) = response.chunk().await.map_err(describe)? {
            if head.len() < 4096 {
                head.push_str(&String::from_utf8_lossy(&bytes));
            }
            buf.extend_from_slice(&bytes);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let Some(chunk) = parse_chunk(&String::from_utf8_lossy(&line)) else { continue };
                saw_data = true;
                if let Some(error) = chunk.error {
                    return Err(format!("API вернул ошибку в потоке: {error}"));
                }
                if chunk.done {
                    continue;
                }
                if !chunk.reasoning.is_empty() {
                    ttft_ms.get_or_insert_with(|| started.elapsed().as_millis());
                    let _ = tx.send(Event::Reasoning(chunk.reasoning)).await;
                }
                if !chunk.content.is_empty() {
                    ttft_ms.get_or_insert_with(|| started.elapsed().as_millis());
                    content.push_str(&chunk.content);
                    let _ = tx.send(Event::Content(chunk.content)).await;
                }
                if let Some(reason) = chunk.finish_reason {
                    finish_reason = Some(reason);
                }
                if let Some(u) = chunk.usage {
                    usage = u;
                }
                if let Some(t) = chunk.completion_time {
                    completion_time = Some(t);
                }
            }
        }

        if !saw_data {
            return Err(format!("вместо потока событий пришло: {}", api_error(head.trim())));
        }
        if content.trim().is_empty() {
            return Err(match finish_reason.as_deref() {
                Some("length") => format!("ответ оборван лимитом {MAX_COMPLETION_TOKENS} токенов"),
                _ => "модель вернула пустой ответ".to_string(),
            });
        }

        let total_ms = started.elapsed().as_millis();
        let metrics = Metrics::build(settings, finish_reason, ttft_ms, total_ms, usage, completion_time);
        Ok((content, metrics))
    }
}

/// Из тела ошибки достаётся человеческая часть; не разобралось — начало тела.
fn api_error(text: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return truncate(text, 300);
    };
    let error = if value["error"].is_object() { &value["error"] } else { &value };
    let message = error["message"].as_str().unwrap_or("").trim();
    if message.is_empty() {
        truncate(text, 300)
    } else {
        truncate(message, 300)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_and_content_deltas_are_separate() {
        let reasoning = parse_chunk(
            r#"data: {"choices":[{"delta":{"reasoning":"надо подумать"},"index":0}]}"#,
        )
        .expect("строка data: разбирается");
        assert_eq!(reasoning.reasoning, "надо подумать");
        assert!(reasoning.content.is_empty());
        assert!(reasoning.usage.is_none());

        let content =
            parse_chunk(r#"data: {"choices":[{"delta":{"content":"Привет"},"index":0}]}"#).unwrap();
        assert_eq!(content.content, "Привет");
        assert!(content.reasoning.is_empty());

        // Не data: и мусор внутри data: — не события, а не паника.
        assert!(parse_chunk(": keep-alive").is_none());
        assert!(parse_chunk("").is_none());
        assert!(parse_chunk("data: {не json}").is_none());
    }

    #[test]
    fn done_marker_is_recognised() {
        let done = parse_chunk("data: [DONE]").expect("маркер конца разбирается");
        assert!(done.done);
        assert_eq!(done, Chunk { done: true, ..Chunk::default() });
    }

    /// Реальный последний чанк Cerebras: счётчики и тайминги приходят с ним.
    #[test]
    fn final_chunk_carries_usage_and_time_info() {
        let line = r#"data: {"id":"chatcmpl-x","choices":[{"delta":{},"finish_reason":"length","index":0}],"created":1788791665,"model":"qwen-3.8-27b","object":"chat.completion.chunk","usage":{"total_tokens":62,"completion_tokens":8,"completion_tokens_details":{"reasoning_tokens":8},"prompt_tokens":54,"prompt_tokens_details":{"cached_tokens":0,"image_tokens":0}},"time_info":{"created":1788791665.212307,"queue_time":0.000216711,"prompt_time":0.003160362,"completion_time":0.002019803,"total_time":0.009371042251586914}}"#;
        let chunk = parse_chunk(line).expect("финальный чанк разбирается");
        assert_eq!(chunk.finish_reason.as_deref(), Some("length"));
        assert_eq!(
            chunk.usage,
            Some(Usage { prompt_tokens: 54, completion_tokens: 8, reasoning_tokens: 8 })
        );
        assert_eq!(chunk.completion_time, Some(0.002019803));
        assert!(chunk.content.is_empty() && chunk.reasoning.is_empty());
    }

    #[test]
    fn cost_follows_the_price_table_and_is_none_without_prices() {
        let usage = Usage { prompt_tokens: 54, completion_tokens: 312, reasoning_tokens: 8 };
        let expected = 54e-6 * 0.99 + 312e-6 * 1.49;
        let actual = cost("qwen-3.8-27b", usage).expect("у qwen есть цена");
        assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");

        // У Gemma цены на странице нет — стоимость не выдумывается.
        assert!(cost("gemma-4-31b", usage).is_none());
        assert!(cost("нет-такой-модели", usage).is_none());
    }

    fn metrics(ttft_ms: Option<u128>, total_ms: u128, completion_time: Option<f64>) -> Metrics {
        Metrics::build(
            &Settings::default(),
            Some("stop".to_string()),
            ttft_ms,
            total_ms,
            Usage { prompt_tokens: 10, completion_tokens: 100, reasoning_tokens: 0 },
            completion_time,
        )
    }

    #[test]
    fn tokens_per_second_needs_a_positive_denominator() {
        let m = metrics(Some(200), 1200, Some(0.05));
        assert_eq!(m.server_tok_s, Some(2000.0));
        // Генерация считается от первой дельты: 1200 − 200 = 1 с на 100 токенов.
        assert_eq!(m.client_tok_s, Some(100.0));

        // Ни один нулевой знаменатель не должен превратиться в бесконечность.
        assert_eq!(metrics(Some(200), 1200, Some(0.0)).server_tok_s, None);
        assert_eq!(metrics(Some(200), 1200, None).server_tok_s, None);
        assert_eq!(metrics(Some(1200), 1200, None).client_tok_s, None);
        assert_eq!(metrics(None, 1200, None).client_tok_s, None);
    }

    #[test]
    fn settings_are_validated_before_they_reach_the_agent() {
        let ok = Settings::default();
        assert_eq!(ok.model, "qwen-3.8-27b");
        assert_eq!(ok.reasoning_effort, "none");
        assert!(ok.validate().is_ok());

        let bad_model = Settings { model: "gpt-5".to_string(), ..Settings::default() };
        assert!(bad_model.validate().is_err());
        for temperature in [-0.1, 2.1] {
            let s = Settings { temperature, ..Settings::default() };
            assert!(s.validate().is_err(), "температура {temperature} должна отклоняться");
        }
        for temperature in [0.0, 2.0] {
            let s = Settings { temperature, ..Settings::default() };
            assert!(s.validate().is_ok(), "температура {temperature} допустима");
        }
        let bad_effort = Settings { reasoning_effort: "max".to_string(), ..Settings::default() };
        assert!(bad_effort.validate().is_err());
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("w2d1-{name}-{}.json", std::process::id()))
    }

    #[test]
    fn settings_survive_a_round_trip_through_a_file() {
        let path = temp_path("roundtrip");
        let file = path.to_str().expect("путь во временный каталог — валидный UTF-8");
        let _ = std::fs::remove_file(&path);

        let saved = Settings {
            model: "gpt-oss-120b".to_string(),
            temperature: 1.3,
            reasoning_effort: "high".to_string(),
            system_prompt: "Ты — Петрович.\nВторая строка.".to_string(),
        };
        saved.save(file).expect("файл записывается");

        let loaded = Settings::load(file);
        assert_eq!(loaded.model, saved.model);
        assert_eq!(loaded.temperature, saved.temperature);
        assert_eq!(loaded.reasoning_effort, saved.reasoning_effort);
        assert_eq!(loaded.system_prompt, saved.system_prompt);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_and_broken_files_fall_back_to_defaults() {
        let path = temp_path("broken");
        let file = path.to_str().unwrap();
        let _ = std::fs::remove_file(&path);

        // Файла нет — умолчания молча.
        assert_eq!(Settings::load(file).model, Settings::default().model);

        // Не JSON и JSON, не проходящий валидацию, — тоже умолчания.
        for text in ["{это не json", r#"{"model":"gpt-5","temperature":0.7,"reasoning_effort":"none","system_prompt":"x"}"#] {
            std::fs::write(&path, text).expect("временный файл записывается");
            let loaded = Settings::load(file);
            assert_eq!(loaded.model, Settings::default().model, "содержимое: {text}");
            assert_eq!(loaded.system_prompt, DEFAULT_SYSTEM_PROMPT);
        }

        let _ = std::fs::remove_file(&path);
    }
}
