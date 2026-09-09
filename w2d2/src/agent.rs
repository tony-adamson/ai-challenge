//! Агент: провайдеры, настройки, сборка запроса и разбор потока. Историю
//! агент не хранит — она лежит в `Chat` (см. `store.rs`), агент её только
//! читает и дописывает. Про HTTP-сервер здесь не знают: наружу торчат
//! `Provider`, `Settings`, `Message`, `Metrics` и поток `Event`.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::store::Chat;

/// Потолок ответа. Больше не нужно: это чат, а не генерация документов.
const MAX_TOKENS: u32 = 4096;
const TIMEOUT: Duration = Duration::from_secs(180);

/// Личность чата: домен, в котором агент учит. Промпт — шаблон: при выборе
/// личности он копируется в `Settings::system_prompt`, и дальше чат правит
/// уже свою копию, а шаблон остаётся тем, к чему можно вернуться.
pub struct Persona {
    pub id: &'static str,
    pub name: &'static str,
    pub prompt: &'static str,
}

/// Первая — личность нового чата.
pub const PERSONAS: &[Persona] = &[
    Persona {
        id: "robotics",
        name: "Робототехник",
        prompt: r#"Ты — Робототехник, преподаватель учебной лаборатории.
Учишь робототехнике: кинематика и динамика, датчики и приводы, ROS 2, управление и ПИД-регуляторы, встроенные контроллеры, безопасность железа.
Объясняешь от простого к сложному: сначала идея на пальцах, потом формула или код.
Говоришь на «ты», спокойно и по делу, без восклицательных знаков.
Отвечаешь по-русски. Код и формулы оформляешь в markdown.
Не выдумываешь: не уверен в цифре, модели датчика или сигнатуре API — говоришь, что не уверен.
Про безопасность предупреждаешь до того, как человек подаст питание на железо.
Когда уместно, предлагаешь маленькое упражнение минут на десять.
В конце объяснения задаёшь один вопрос на понимание."#,
    },
    Persona {
        id: "ml",
        name: "ML-инженер",
        prompt: r#"Ты — ML-инженер, преподаватель учебной лаборатории.
Учишь машинному обучению: данные и разметка, классические методы и нейросети, обучение и переобучение, метрики и валидация, PyTorch, разбор ошибок экспериментов.
Объясняешь от простого к сложному: сначала зачем это нужно, потом как считается.
Говоришь на «ты», спокойно и по делу, без восклицательных знаков.
Отвечаешь по-русски. Код и формулы оформляешь в markdown.
Начинаешь с данных и метрики: без них разговор про модель бессмысленный.
Не выдумываешь: не помнишь точное число из статьи или сигнатуру функции — говоришь, что не уверен.
Когда уместно, предлагаешь маленький эксперимент, который человек прогонит сам.
В конце объяснения задаёшь один вопрос на понимание."#,
    },
    Persona {
        id: "ai",
        name: "ИИ-инженер",
        prompt: r#"Ты — ИИ-инженер, преподаватель учебной лаборатории.
Учишь работе с большими языковыми моделями: как они устроены и где их границы, промпты, агенты и инструменты, RAG, оценка качества, стоимость и латентность, безопасность.
Объясняешь от простого к сложному: сначала что происходит внутри, потом как это применить.
Говоришь на «ты», спокойно и по делу, без восклицательных знаков.
Отвечаешь по-русски. Код, промпты и схемы оформляешь в markdown.
Про цену и задержку говоришь так же серьёзно, как про качество ответа.
Не выдумываешь: не уверен в поведении конкретной модели или в цифрах прайса — говоришь, что не уверен.
Когда уместно, предлагаешь маленькое упражнение: переписать промпт, померить, сравнить.
В конце объяснения задаёшь один вопрос на понимание."#,
    },
    Persona {
        id: "chips",
        name: "Схемотехник",
        prompt: r#"Ты — Схемотехник, преподаватель учебной лаборатории.
Учишь проектированию микросхем: цифровая логика, Verilog и SystemVerilog, FPGA, маршрут ASIC от RTL через синтез к размещению и трассировке, тайминги, основы аналоговой схемотехники, чтение даташитов.
Объясняешь от простого к сложному: сначала что делает схема, потом как описать её в коде.
Говоришь на «ты», спокойно и по делу, без восклицательных знаков.
Отвечаешь по-русски. Код и временные диаграммы оформляешь в markdown.
Не выдумываешь: не уверен в параметре из даташита или в поведении конкретной ПЛИС — говоришь, что не уверен.
Когда уместно, предлагаешь маленькое упражнение: описать модуль, посчитать задержку, прочитать страницу даташита.
В конце объяснения задаёшь один вопрос на понимание."#,
    },
    Persona {
        id: "free",
        name: "Свободный",
        prompt: r#"Ты — ассистент учебной лаборатории без своей узкой темы.
Отвечаешь на любой вопрос коротко и по делу, роль на себя не берёшь.
Объясняешь от простого к сложному: сначала суть в двух фразах, потом детали.
Говоришь на «ты», спокойно, без восклицательных знаков.
Отвечаешь по-русски. Код и формулы оформляешь в markdown.
Не выдумываешь: не знаешь или не уверен — говоришь об этом прямо.
Длинный ответ пишешь только там, где вопрос действительно требует разбора.
Когда уместно, предлагаешь маленькое упражнение или следующий шаг.
В конце объяснения задаёшь один вопрос на понимание, если он к месту."#,
    },
];

/// Личность старых чатов, сохранённых до появления поля.
const FALLBACK_PERSONA: &str = "free";

pub fn persona(id: &str) -> Option<&'static Persona> {
    PERSONAS.iter().find(|p| p.id == id)
}

pub struct ModelInfo {
    pub id: &'static str,
    pub label: &'static str,
    /// Заявленная скорость генерации, токенов в секунду — ориентир из
    /// документации провайдера, а не измерение. 0 — не заявлена.
    pub tok_s_hint: u32,
    /// Доллары за 1M токенов входа и выхода по прайсу провайдера.
    /// `None` — цены на странице нет, считать стоимость не из чего.
    pub price_in: Option<f64>,
    pub price_out: Option<f64>,
}

const CEREBRAS_MODELS: [ModelInfo; 3] = [
    ModelInfo { id: "qwen-3.8-27b", label: "Qwen 3.8 27B", tok_s_hint: 1500, price_in: Some(0.99), price_out: Some(1.49) },
    ModelInfo { id: "gpt-oss-120b", label: "GPT-OSS 120B", tok_s_hint: 3000, price_in: Some(0.35), price_out: Some(0.75) },
    ModelInfo { id: "gemma-4-31b", label: "Gemma 4 31B", tok_s_hint: 0, price_in: None, price_out: None },
];

/// Цены DeepSeek — по прайсу вне пика (в часы скидки они ниже).
const DEEPSEEK_MODELS: [ModelInfo; 2] = [
    ModelInfo { id: "deepseek-v4-flash", label: "DeepSeek V4 Flash", tok_s_hint: 0, price_in: Some(0.22), price_out: Some(0.66) },
    ModelInfo { id: "deepseek-v4-pro", label: "DeepSeek V4 Pro", tok_s_hint: 0, price_in: Some(0.66), price_out: Some(1.98) },
];

/// У Cerebras глубина рассуждения — `reasoning_effort`, у DeepSeek оно
/// включено по умолчанию и гасится отдельным полем `thinking`. Наружу
/// разница не торчит: и там и там уровень выбирается из этого списка.
const CEREBRAS_REASONING: [(&str, &str); 4] =
    [("none", "выключено"), ("low", "низкое"), ("medium", "среднее"), ("high", "высокое")];
const DEEPSEEK_REASONING: [(&str, &str); 4] =
    [("none", "выключено"), ("low", "низкое"), ("high", "высокое"), ("max", "максимум")];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    Cerebras,
    DeepSeek,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::Cerebras, Provider::DeepSeek];

    pub fn from_id(id: &str) -> Option<Provider> {
        Provider::ALL.into_iter().find(|p| p.id() == id)
    }

    pub fn id(&self) -> &'static str {
        match self {
            Provider::Cerebras => "cerebras",
            Provider::DeepSeek => "deepseek",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Provider::Cerebras => "Cerebras",
            Provider::DeepSeek => "DeepSeek",
        }
    }

    pub fn base_url(&self) -> &'static str {
        match self {
            Provider::Cerebras => "https://api.cerebras.ai/v1/chat/completions",
            Provider::DeepSeek => "https://api.deepseek.com/chat/completions",
        }
    }

    pub fn key_env(&self) -> &'static str {
        match self {
            Provider::Cerebras => "CEREBRAS_API_KEY",
            Provider::DeepSeek => "DEEPSEEK_API_KEY",
        }
    }

    pub fn models(&self) -> &'static [ModelInfo] {
        match self {
            Provider::Cerebras => &CEREBRAS_MODELS,
            Provider::DeepSeek => &DEEPSEEK_MODELS,
        }
    }

    pub fn reasoning_levels(&self) -> &'static [(&'static str, &'static str)] {
        match self {
            Provider::Cerebras => &CEREBRAS_REASONING,
            Provider::DeepSeek => &DEEPSEEK_REASONING,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    /// `cerebras` | `deepseek`.
    pub provider: String,
    pub model: String,
    pub temperature: f64,
    /// Уровень из `Provider::reasoning_levels`. У qwen-3.8-27b на стороне
    /// Cerebras и у DeepSeek умолчание — рассуждать, поэтому уровень всегда
    /// отправляется явно.
    pub reasoning: String,
    /// id из `PERSONAS`. Чаты, сохранённые до появления личностей, поля не
    /// знают — им достаётся нейтральная: чужой домен навязывать нечестно.
    #[serde(default = "fallback_persona")]
    pub persona: String,
    /// Текст промпта самого чата: копия шаблона личности, которую можно
    /// править. Личность здесь только помечает, откуда копия взялась.
    pub system_prompt: String,
}

fn fallback_persona() -> String {
    FALLBACK_PERSONA.to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            provider: Provider::Cerebras.id().to_string(),
            model: CEREBRAS_MODELS[0].id.to_string(),
            temperature: 0.7,
            reasoning: "none".to_string(),
            persona: PERSONAS[0].id.to_string(),
            system_prompt: PERSONAS[0].prompt.to_string(),
        }
    }
}

impl Settings {
    pub fn provider(&self) -> Result<Provider, String> {
        Provider::from_id(&self.provider).ok_or(format!("неизвестный провайдер: {}", self.provider))
    }

    pub fn validate(&self) -> Result<(), String> {
        let provider = self.provider()?;
        if !provider.models().iter().any(|m| m.id == self.model) {
            return Err(format!("у {} нет модели {}", provider.label(), self.model));
        }
        if !(0.0..=2.0).contains(&self.temperature) {
            return Err("температура вне диапазона 0–2".to_string());
        }
        if !provider.reasoning_levels().iter().any(|(value, _)| *value == self.reasoning) {
            return Err(format!("неизвестный режим рассуждения: {}", self.reasoning));
        }
        if persona(&self.persona).is_none() {
            return Err(format!("неизвестная личность: {}", self.persona));
        }
        Ok(())
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
    /// Вход, попавший в кэш провайдера. У DeepSeek он почти бесплатен, у
    /// Cerebras такого поля нет — там всегда 0.
    pub cached_prompt_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Metrics {
    pub provider: String,
    pub model: String,
    pub reasoning: String,
    pub finish_reason: Option<String>,
    /// Клиентские замеры: от отправки запроса до первой дельты и до конца.
    pub ttft_ms: Option<u128>,
    pub total_ms: u128,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub cached_prompt_tokens: u64,
    /// Скорость по `time_info.completion_time` самого Cerebras — без сети.
    /// DeepSeek таких таймингов не присылает, там всегда `None`.
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
            provider: settings.provider.clone(),
            model: settings.model.clone(),
            reasoning: settings.reasoning.clone(),
            finish_reason,
            ttft_ms,
            total_ms,
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            cached_prompt_tokens: usage.cached_prompt_tokens,
            server_tok_s,
            client_tok_s,
            cost_usd: settings.provider().ok().and_then(|p| cost(p, &settings.model, usage)),
        }
    }
}

/// Стоимость вызова по прайсу. Токены рассуждения входят в
/// `completion_tokens` и тарифицируются как выход — отдельно их не считаем.
/// Вход, пришедший из кэша, у DeepSeek считается по нулю: платить за него
/// как за обычный вход значило бы завышать счёт в разы на длинном чате.
pub fn cost(provider: Provider, model: &str, usage: Usage) -> Option<f64> {
    let info = provider.models().iter().find(|m| m.id == model)?;
    let (price_in, price_out) = (info.price_in?, info.price_out?);
    let billed_in = usage.prompt_tokens.saturating_sub(usage.cached_prompt_tokens);
    Some(billed_in as f64 * price_in / 1e6 + usage.completion_tokens as f64 * price_out / 1e6)
}

#[derive(Debug)]
pub enum Event {
    Reasoning(String),
    Content(String),
    Done(Metrics),
    /// Тема разговора, придуманная моделью после первого обмена.
    Title(String),
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
    /// `time_info.completion_time`, секунды. Только Cerebras.
    pub completion_time: Option<f64>,
    /// Строка `data: [DONE]` — конец потока.
    pub done: bool,
    /// Ошибка, пришедшая внутри потока (статус при этом 200).
    pub error: Option<String>,
}

/// Разбор одинаков для обоих провайдеров: поля, которых у провайдера нет,
/// просто не находятся. Рассуждение Cerebras зовётся `reasoning`, у
/// DeepSeek — `reasoning_content`; кэш входа есть только у DeepSeek.
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
    let reasoning = delta["reasoning"]
        .as_str()
        .or_else(|| delta["reasoning_content"].as_str())
        .unwrap_or("");
    let usage = &value["usage"];
    Some(Chunk {
        reasoning: reasoning.to_string(),
        content: delta["content"].as_str().unwrap_or("").to_string(),
        finish_reason: value["choices"][0]["finish_reason"].as_str().map(String::from),
        usage: usage.is_object().then(|| Usage {
            prompt_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0),
            completion_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
            reasoning_tokens: usage["completion_tokens_details"]["reasoning_tokens"]
                .as_u64()
                .unwrap_or(0),
            cached_prompt_tokens: usage["prompt_cache_hit_tokens"].as_u64().unwrap_or(0),
        }),
        completion_time: value["time_info"]["completion_time"].as_f64(),
        done: false,
        error: None,
    })
}

/// Тело запроса. Общая часть — формат OpenAI Chat Completions; расходятся
/// провайдеры на потолке ответа, управлении рассуждением и на том, кто
/// присылает `usage` в стриме без спроса.
fn request_body(provider: Provider, settings: &Settings, messages: &[Message]) -> Value {
    let mut body = json!({
        "model": settings.model,
        "messages": messages,
        "stream": true,
        "temperature": settings.temperature,
    });
    let map = body.as_object_mut().expect("собран как объект");
    match provider {
        Provider::Cerebras => {
            map.insert("max_completion_tokens".to_string(), json!(MAX_TOKENS));
            map.insert("reasoning_effort".to_string(), json!(settings.reasoning));
        }
        Provider::DeepSeek => {
            map.insert("max_tokens".to_string(), json!(MAX_TOKENS));
            // Без этого DeepSeek не присылает usage последним чанком, и
            // считать стоимость было бы не из чего.
            map.insert("stream_options".to_string(), json!({ "include_usage": true }));
            if settings.reasoning == "none" {
                map.insert("thinking".to_string(), json!({ "type": "disabled" }));
            } else {
                map.insert("thinking".to_string(), json!({ "type": "enabled" }));
                map.insert("reasoning_effort".to_string(), json!(settings.reasoning));
            }
        }
    }
    body
}

/// Промпт для темы чата. Отдельный дешёвый вызов: рассуждение выключено,
/// потолок в два десятка токенов, ответ не стримится.
const TITLE_PROMPT: &str =
    "Сформулируй тему разговора в 2–5 словах на языке собеседника. Только тема, без кавычек, точки и пояснений.";
const TITLE_MAX_TOKENS: u32 = 24;
/// Сколько символов первого вопроса и первого ответа отдаём модели: тема
/// видна по началу разговора, платить за весь ответ незачем.
const TITLE_SOURCE_LIMIT: usize = 600;
/// Потолок самой темы. Длиннее в список чатов всё равно не влезет.
const TITLE_LIMIT: usize = 60;

fn title_body(provider: Provider, model: &str, question: &str, answer: &str) -> Value {
    let user = format!(
        "Вопрос: {}\n\nОтвет: {}",
        head(question, TITLE_SOURCE_LIMIT),
        head(answer, TITLE_SOURCE_LIMIT)
    );
    let mut body = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": TITLE_PROMPT },
            { "role": "user", "content": user },
        ],
        "stream": false,
        "temperature": 0.3,
    });
    let map = body.as_object_mut().expect("собран как объект");
    match provider {
        Provider::Cerebras => {
            map.insert("max_completion_tokens".to_string(), json!(TITLE_MAX_TOKENS));
            map.insert("reasoning_effort".to_string(), json!("none"));
        }
        Provider::DeepSeek => {
            map.insert("max_tokens".to_string(), json!(TITLE_MAX_TOKENS));
            map.insert("thinking".to_string(), json!({ "type": "disabled" }));
        }
    }
    body
}

/// Модель просили отдать голую тему, но кавычки и точку она всё равно
/// иногда ставит. Пусто — темы нет, заголовок останется прежним.
fn clean_title(raw: &str) -> Option<String> {
    let unquoted = raw.trim().trim_matches(|c| matches!(c, '"' | '\'' | '`' | '«' | '»'));
    let text = unquoted.trim().trim_end_matches('.').trim();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return None;
    }
    Some(text.chars().take(TITLE_LIMIT).collect())
}

fn head(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

pub struct Agent {
    client: Client,
    /// Ключи тех провайдеров, что нашлись в окружении. Провайдер без ключа
    /// не исчезает из интерфейса — он помечен недоступным с причиной.
    keys: HashMap<Provider, String>,
    /// Занятые чаты. Занятость на чат, а не на агента: два разных чата
    /// могут отвечать одновременно, один и тот же — нет.
    busy: Mutex<HashSet<String>>,
}

impl Agent {
    /// Ключи читаются из окружения (`.env` подхватывает `main`). Ни одного
    /// ключа — запускаться незачем.
    pub fn new() -> Result<Agent, String> {
        let keys: HashMap<Provider, String> = Provider::ALL
            .into_iter()
            .filter_map(|p| {
                let key = std::env::var(p.key_env()).ok().filter(|v| !v.trim().is_empty())?;
                Some((p, key))
            })
            .collect();
        if keys.is_empty() {
            return Err(
                "не задан ни один ключ (CEREBRAS_API_KEY, DEEPSEEK_API_KEY): скопируй .env.example в .env"
                    .to_string(),
            );
        }
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| format!("не удалось создать HTTP-клиент: {e}"))?;
        Ok(Agent { client, keys, busy: Mutex::new(HashSet::new()) })
    }

    pub fn is_available(&self, provider: Provider) -> bool {
        self.keys.contains_key(&provider)
    }

    /// Первый провайдер с ключом — на нём открывается новый чат, если
    /// наследовать настройки не у кого.
    pub fn default_settings(&self) -> Settings {
        let settings = Settings::default();
        if settings.provider().is_ok_and(|p| self.is_available(p)) {
            return settings;
        }
        match Provider::ALL.into_iter().find(|p| self.is_available(*p)) {
            Some(p) => Settings {
                provider: p.id().to_string(),
                model: p.models()[0].id.to_string(),
                ..settings
            },
            None => settings,
        }
    }

    fn key(&self, provider: Provider) -> Result<&String, String> {
        self.keys
            .get(&provider)
            .ok_or(format!("нет {} в .env — провайдер {} недоступен", provider.key_env(), provider.label()))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.busy.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Один вопрос: дописывает его в чат, стримит дельты в `tx` и по
    /// завершении кладёт в чат текст ответа (без рассуждения). Сохранение
    /// на диск — забота вызывающего: агент не знает, где лежат файлы.
    ///
    /// Рассуждение назад в историю не отправляется: по документации обоих
    /// провайдеров оно относится к одному ответу и в следующем запросе
    /// игнорируется.
    pub async fn ask(
        &self,
        chat: &mut Chat,
        text: &str,
        tx: &mpsc::Sender<Event>,
    ) -> Result<Metrics, String> {
        let text = text.trim().to_string();
        if text.is_empty() {
            return Err("пустой запрос".to_string());
        }
        chat.settings.validate()?;
        let provider = chat.settings.provider()?;
        let key = self.key(provider)?.clone();

        if !self.lock().insert(chat.id.clone()) {
            return Err("чат занят: дождись конца предыдущего ответа".to_string());
        }
        chat.push_user(&text);

        let mut messages = vec![Message {
            role: "system".to_string(),
            content: chat.settings.system_prompt.clone(),
        }];
        messages.extend(chat.messages.iter().cloned());

        let outcome = self.stream(provider, &key, &chat.settings, &messages, tx).await;
        self.lock().remove(&chat.id);

        match outcome {
            Ok((answer, metrics)) => {
                chat.push_assistant(answer);
                Ok(metrics)
            }
            Err(error) => {
                chat.pop_user(&text);
                Err(error)
            }
        }
    }

    /// Тема разговора по первому обмену — одним дешёвым запросом к тому же
    /// провайдеру и той же модели. Зовётся один раз за жизнь чата, поэтому
    /// любая осечка (нет ключа, сеть, пустой ответ) — просто `None`:
    /// заголовок из первого вопроса уже есть, и терять из-за темы нечего.
    /// В метрики этот вызов не попадает — он не ответ на вопрос человека.
    pub async fn title(&self, chat: &Chat) -> Option<String> {
        let provider = chat.settings.provider().ok()?;
        let key = self.keys.get(&provider)?;
        let [question, answer, ..] = chat.messages.as_slice() else { return None };

        let body = title_body(provider, &chat.settings.model, &question.content, &answer.content);
        let response = self
            .client
            .post(provider.base_url())
            .bearer_auth(key)
            .json(&body)
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let value: Value = response.json().await.ok()?;
        clean_title(value["choices"][0]["message"]["content"].as_str()?)
    }

    async fn stream(
        &self,
        provider: Provider,
        key: &str,
        settings: &Settings,
        messages: &[Message],
        tx: &mpsc::Sender<Event>,
    ) -> Result<(String, Metrics), String> {
        let started = Instant::now();
        let mut response = self
            .client
            .post(provider.base_url())
            .bearer_auth(key)
            .json(&request_body(provider, settings, messages))
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
                Some("length") => format!("ответ оборван лимитом {MAX_TOKENS} токенов"),
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

    /// У DeepSeek рассуждение приезжает в другом поле — разбор один на двоих.
    #[test]
    fn deepseek_reasoning_content_lands_in_the_same_field() {
        let line = r#"data: {"id":"a","choices":[{"index":0,"delta":{"content":null,"reasoning_content":"прикидываю"},"finish_reason":null}],"model":"deepseek-v4-flash","object":"chat.completion.chunk"}"#;
        let chunk = parse_chunk(line).expect("чанк DeepSeek разбирается");
        assert_eq!(chunk.reasoning, "прикидываю");
        assert!(chunk.content.is_empty());
        assert_eq!(chunk.completion_time, None, "time_info DeepSeek не присылает");
    }

    /// Реальный последний чанк Cerebras: счётчики и тайминги приходят с ним.
    #[test]
    fn final_chunk_carries_usage_and_time_info() {
        let line = r#"data: {"id":"chatcmpl-x","choices":[{"delta":{},"finish_reason":"length","index":0}],"created":1788791665,"model":"qwen-3.8-27b","object":"chat.completion.chunk","usage":{"total_tokens":62,"completion_tokens":8,"completion_tokens_details":{"reasoning_tokens":8},"prompt_tokens":54,"prompt_tokens_details":{"cached_tokens":0,"image_tokens":0}},"time_info":{"created":1788791665.212307,"queue_time":0.000216711,"prompt_time":0.003160362,"completion_time":0.002019803,"total_time":0.009371042251586914}}"#;
        let chunk = parse_chunk(line).expect("финальный чанк разбирается");
        assert_eq!(chunk.finish_reason.as_deref(), Some("length"));
        assert_eq!(
            chunk.usage,
            Some(Usage {
                prompt_tokens: 54,
                completion_tokens: 8,
                reasoning_tokens: 8,
                cached_prompt_tokens: 0
            })
        );
        assert_eq!(chunk.completion_time, Some(0.002019803));
        assert!(chunk.content.is_empty() && chunk.reasoning.is_empty());
    }

    #[test]
    fn deepseek_usage_reports_the_cache_hit() {
        let line = r#"data: {"id":"b","choices":[],"model":"deepseek-v4-flash","object":"chat.completion.chunk","usage":{"prompt_tokens":1000,"completion_tokens":50,"total_tokens":1050,"prompt_cache_hit_tokens":900,"prompt_cache_miss_tokens":100}}"#;
        let usage = parse_chunk(line).expect("чанк с usage разбирается").usage.expect("usage есть");
        assert_eq!(usage.prompt_tokens, 1000);
        assert_eq!(usage.cached_prompt_tokens, 900);
    }

    #[test]
    fn done_marker_is_recognised() {
        let done = parse_chunk("data: [DONE]").expect("маркер конца разбирается");
        assert!(done.done);
        assert_eq!(done, Chunk { done: true, ..Chunk::default() });
    }

    #[test]
    fn cost_follows_the_price_table_and_is_none_without_prices() {
        let usage = Usage { prompt_tokens: 54, completion_tokens: 312, reasoning_tokens: 8, cached_prompt_tokens: 0 };
        let expected = 54e-6 * 0.99 + 312e-6 * 1.49;
        let actual = cost(Provider::Cerebras, "qwen-3.8-27b", usage).expect("у qwen есть цена");
        assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");

        // У Gemma цены на странице нет — стоимость не выдумывается.
        assert!(cost(Provider::Cerebras, "gemma-4-31b", usage).is_none());
        assert!(cost(Provider::Cerebras, "нет-такой-модели", usage).is_none());
        // Модель чужого провайдера не считается по своему прайсу.
        assert!(cost(Provider::Cerebras, "deepseek-v4-pro", usage).is_none());
    }

    #[test]
    fn deepseek_cache_hit_is_billed_at_zero() {
        let usage = Usage {
            prompt_tokens: 1000,
            completion_tokens: 200,
            reasoning_tokens: 0,
            cached_prompt_tokens: 900,
        };
        // Платим за 100 токенов входа из 1000 и за весь выход.
        let expected = 100e-6 * 0.22 + 200e-6 * 0.66;
        let actual = cost(Provider::DeepSeek, "deepseek-v4-flash", usage).expect("цена есть");
        assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");

        // Без кэша тот же вызов стоит заметно дороже.
        let no_cache = cost(
            Provider::DeepSeek,
            "deepseek-v4-flash",
            Usage { cached_prompt_tokens: 0, ..usage },
        )
        .unwrap();
        assert!(no_cache > actual);
    }

    fn metrics(ttft_ms: Option<u128>, total_ms: u128, completion_time: Option<f64>) -> Metrics {
        Metrics::build(
            &Settings::default(),
            Some("stop".to_string()),
            ttft_ms,
            total_ms,
            Usage { prompt_tokens: 10, completion_tokens: 100, reasoning_tokens: 0, cached_prompt_tokens: 0 },
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
    fn settings_are_validated_against_their_provider() {
        let ok = Settings::default();
        assert_eq!(ok.provider, "cerebras");
        assert_eq!(ok.model, "qwen-3.8-27b");
        assert_eq!(ok.reasoning, "none");
        assert!(ok.validate().is_ok());

        let bad_provider = Settings { provider: "openai".to_string(), ..Settings::default() };
        assert!(bad_provider.validate().is_err());

        // Модель существует, но у другого провайдера — тоже отказ.
        let mixed = Settings { model: "deepseek-v4-pro".to_string(), ..Settings::default() };
        assert!(mixed.validate().is_err());

        for temperature in [-0.1, 2.1] {
            let s = Settings { temperature, ..Settings::default() };
            assert!(s.validate().is_err(), "температура {temperature} должна отклоняться");
        }
        for temperature in [0.0, 2.0] {
            let s = Settings { temperature, ..Settings::default() };
            assert!(s.validate().is_ok(), "температура {temperature} допустима");
        }

        // «medium» есть у Cerebras и нет у DeepSeek, «max» — наоборот.
        let cerebras_medium = Settings { reasoning: "medium".to_string(), ..Settings::default() };
        assert!(cerebras_medium.validate().is_ok());
        let cerebras_max = Settings { reasoning: "max".to_string(), ..Settings::default() };
        assert!(cerebras_max.validate().is_err());
        let deepseek = Settings {
            provider: "deepseek".to_string(),
            model: "deepseek-v4-flash".to_string(),
            reasoning: "medium".to_string(),
            ..Settings::default()
        };
        assert!(deepseek.validate().is_err());
        assert!(Settings { reasoning: "max".to_string(), ..deepseek }.validate().is_ok());
    }

    #[test]
    fn request_body_matches_each_provider() {
        let messages = [Message { role: "user".to_string(), content: "привет".to_string() }];

        let cerebras = request_body(Provider::Cerebras, &Settings::default(), &messages);
        assert_eq!(cerebras["max_completion_tokens"], json!(MAX_TOKENS));
        assert_eq!(cerebras["reasoning_effort"], json!("none"));
        assert!(cerebras.get("thinking").is_none());
        assert!(cerebras.get("stream_options").is_none());

        let off = Settings {
            provider: "deepseek".to_string(),
            model: "deepseek-v4-flash".to_string(),
            reasoning: "none".to_string(),
            ..Settings::default()
        };
        let body = request_body(Provider::DeepSeek, &off, &messages);
        assert_eq!(body["max_tokens"], json!(MAX_TOKENS));
        assert!(body.get("max_completion_tokens").is_none());
        assert_eq!(body["stream_options"]["include_usage"], json!(true));
        assert_eq!(body["thinking"]["type"], json!("disabled"));
        assert!(body.get("reasoning_effort").is_none(), "выключенное рассуждение уровня не имеет");

        let on = Settings { reasoning: "high".to_string(), ..off };
        let body = request_body(Provider::DeepSeek, &on, &messages);
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert_eq!(body["reasoning_effort"], json!("high"));
    }

    #[test]
    fn persona_is_validated_and_default_chat_starts_with_the_first_one() {
        let ok = Settings::default();
        assert_eq!(ok.persona, "robotics");
        assert_eq!(ok.system_prompt, PERSONAS[0].prompt, "новый чат берёт шаблон личности");
        assert!(ok.validate().is_ok());

        for id in ["robotics", "ml", "ai", "chips", "free"] {
            let s = Settings { persona: id.to_string(), ..Settings::default() };
            assert!(s.validate().is_ok(), "личность {id} должна существовать");
        }
        for id in ["x", "", "Робототехник"] {
            let s = Settings { persona: id.to_string(), ..Settings::default() };
            assert!(s.validate().is_err(), "личность {id} должна отклоняться");
        }
        assert!(persona("ml").is_some_and(|p| p.name == "ML-инженер"));
        assert!(persona("нет-такой").is_none());
    }

    /// Чаты, сохранённые до появления личностей, должны читаться дальше.
    #[test]
    fn old_settings_without_persona_fall_back_to_free() {
        let json = r#"{"provider":"cerebras","model":"qwen-3.8-27b","temperature":0.7,
            "reasoning":"none","system_prompt":"старый промпт"}"#;
        let settings: Settings = serde_json::from_str(json).expect("старые настройки читаются");
        assert_eq!(settings.persona, "free");
        assert_eq!(settings.system_prompt, "старый промпт", "свой промпт чата не трогаем");
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn title_is_stripped_of_quotes_dots_and_excess_length() {
        assert_eq!(clean_title("  ПИД-регулятор  "), Some("ПИД-регулятор".to_string()));
        assert_eq!(clean_title("«Настройка ПИД»"), Some("Настройка ПИД".to_string()));
        assert_eq!(clean_title("\"Настройка ПИД.\""), Some("Настройка ПИД".to_string()));
        assert_eq!(clean_title("Настройка ПИД..."), Some("Настройка ПИД".to_string()));
        assert_eq!(clean_title("Настройка\n  ПИД"), Some("Настройка ПИД".to_string()));

        assert_eq!(clean_title(""), None);
        assert_eq!(clean_title("   "), None);
        assert_eq!(clean_title("\"\""), None);
        assert_eq!(clean_title("."), None);

        let long = clean_title(&"а".repeat(200)).expect("длинная тема не пустая");
        assert_eq!(long.chars().count(), TITLE_LIMIT);
    }

    #[test]
    fn title_request_asks_for_a_short_answer_without_reasoning() {
        let question = "б".repeat(1000);
        let cerebras = title_body(Provider::Cerebras, "qwen-3.8-27b", &question, "ответ");
        assert_eq!(cerebras["stream"], json!(false));
        assert_eq!(cerebras["temperature"], json!(0.3));
        assert_eq!(cerebras["max_completion_tokens"], json!(TITLE_MAX_TOKENS));
        assert_eq!(cerebras["reasoning_effort"], json!("none"));
        assert!(cerebras.get("thinking").is_none());
        assert_eq!(cerebras["messages"][0]["content"], json!(TITLE_PROMPT));

        // Длинный вопрос обрезается: за весь его текст платить незачем.
        let user = cerebras["messages"][1]["content"].as_str().expect("вторая реплика — строка");
        assert!(user.contains("ответ"));
        assert_eq!(user.matches('б').count(), TITLE_SOURCE_LIMIT);

        let deepseek = title_body(Provider::DeepSeek, "deepseek-v4-flash", "вопрос", "ответ");
        assert_eq!(deepseek["stream"], json!(false));
        assert_eq!(deepseek["max_tokens"], json!(TITLE_MAX_TOKENS));
        assert!(deepseek.get("max_completion_tokens").is_none());
        assert_eq!(deepseek["thinking"]["type"], json!("disabled"));
        assert!(deepseek.get("reasoning_effort").is_none());
    }
}
