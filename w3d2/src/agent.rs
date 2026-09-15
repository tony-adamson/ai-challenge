//! Агент: провайдеры, настройки, сборка запроса по слоям памяти и разбор
//! потока. Память агент не хранит — её слои лежат в файлах (см. `store.rs`),
//! агент их только читает и дописывает. Профиль пользователя — такой же вход
//! запроса, как и память, и подставляется сразу за личностью. Про HTTP-сервер
//! здесь не знают: наружу торчат `Provider`, `Settings`, `Message`, `Metrics`
//! и поток `Event`.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::store::{same_key, Chat, Entry, Fact, Kind, LongTerm, Note, Profile, Working};

/// Потолок ответа для моделей, которых нет в справочнике. Свой потолок
/// каждой модели лежит в `ModelInfo::max_tokens`: он входит в окно контекста
/// наравне с промптом, и у маленькой модели его приходится ужимать.
const DEFAULT_MAX_TOKENS: u32 = 4096;
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
    /// Окно контекста: сколько токенов модель принимает за один запрос.
    /// В него входит всё — системный промпт, вся история и потолок ответа.
    pub context_window: u32,
    /// Потолок ответа. Он режется от того же окна, поэтому у 4k-модели его
    /// приходится держать маленьким: иначе промпт плюс потолок не влезают.
    pub max_tokens: u32,
}

const CEREBRAS_MODELS: [ModelInfo; 3] = [
    ModelInfo { id: "qwen-3.8-27b", label: "Qwen 3.8 27B", tok_s_hint: 1500, price_in: Some(0.99), price_out: Some(1.49), context_window: 65_536, max_tokens: 4096 },
    ModelInfo { id: "gpt-oss-120b", label: "GPT-OSS 120B", tok_s_hint: 3000, price_in: Some(0.35), price_out: Some(0.75), context_window: 65_536, max_tokens: 4096 },
    ModelInfo { id: "gemma-4-31b", label: "Gemma 4 31B", tok_s_hint: 0, price_in: None, price_out: None, context_window: 32_768, max_tokens: 4096 },
];

/// Цены DeepSeek — по прайсу вне пика (в часы скидки они ниже).
const DEEPSEEK_MODELS: [ModelInfo; 2] = [
    ModelInfo { id: "deepseek-v4-flash", label: "DeepSeek V4 Flash", tok_s_hint: 0, price_in: Some(0.22), price_out: Some(0.66), context_window: 1_000_000, max_tokens: 4096 },
    ModelInfo { id: "deepseek-v4-pro", label: "DeepSeek V4 Pro", tok_s_hint: 0, price_in: Some(0.66), price_out: Some(1.98), context_window: 1_000_000, max_tokens: 4096 },
];

/// Через OpenRouter взяты две заведомо тесные модели: на них переполнение
/// контекста видно за пару сообщений, а не за час разговора. Цены здесь
/// только для подсказки в панели — фактическую стоимость вызова OpenRouter
/// присылает сам, в `usage.cost`.
const OPENROUTER_MODELS: [ModelInfo; 2] = [
    ModelInfo { id: "openai/gpt-3.5-turbo-0613", label: "GPT-3.5 Turbo 0613 · 4k", tok_s_hint: 0, price_in: Some(1.0), price_out: Some(2.0), context_window: 4095, max_tokens: 1024 },
    ModelInfo { id: "gryphe/mythomax-l2-13b", label: "MythoMax 13B · 8k", tok_s_hint: 0, price_in: Some(0.06), price_out: Some(0.06), context_window: 8192, max_tokens: 2048 },
];

/// У Cerebras глубина рассуждения — `reasoning_effort`, у DeepSeek оно
/// включено по умолчанию и гасится отдельным полем `thinking`. Наружу
/// разница не торчит: и там и там уровень выбирается из этого списка.
const CEREBRAS_REASONING: [(&str, &str); 4] =
    [("none", "выключено"), ("low", "низкое"), ("medium", "среднее"), ("high", "высокое")];
const DEEPSEEK_REASONING: [(&str, &str); 4] =
    [("none", "выключено"), ("low", "низкое"), ("high", "высокое"), ("max", "максимум")];
/// Обе модели OpenRouter рассуждать не умеют — уровень остаётся один.
const OPENROUTER_REASONING: [(&str, &str); 1] = [("none", "выключено")];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    Cerebras,
    DeepSeek,
    OpenRouter,
}

impl Provider {
    pub const ALL: [Provider; 3] = [Provider::Cerebras, Provider::DeepSeek, Provider::OpenRouter];

    pub fn from_id(id: &str) -> Option<Provider> {
        Provider::ALL.into_iter().find(|p| p.id() == id)
    }

    pub fn id(&self) -> &'static str {
        match self {
            Provider::Cerebras => "cerebras",
            Provider::DeepSeek => "deepseek",
            Provider::OpenRouter => "openrouter",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Provider::Cerebras => "Cerebras",
            Provider::DeepSeek => "DeepSeek",
            Provider::OpenRouter => "OpenRouter",
        }
    }

    pub fn base_url(&self) -> &'static str {
        match self {
            Provider::Cerebras => "https://api.cerebras.ai/v1/chat/completions",
            Provider::DeepSeek => "https://api.deepseek.com/chat/completions",
            Provider::OpenRouter => "https://openrouter.ai/api/v1/chat/completions",
        }
    }

    pub fn key_env(&self) -> &'static str {
        match self {
            Provider::Cerebras => "CEREBRAS_API_KEY",
            Provider::DeepSeek => "DEEPSEEK_API_KEY",
            Provider::OpenRouter => "OPENROUTER_API_KEY",
        }
    }

    pub fn models(&self) -> &'static [ModelInfo] {
        match self {
            Provider::Cerebras => &CEREBRAS_MODELS,
            Provider::DeepSeek => &DEEPSEEK_MODELS,
            Provider::OpenRouter => &OPENROUTER_MODELS,
        }
    }

    pub fn reasoning_levels(&self) -> &'static [(&'static str, &'static str)] {
        match self {
            Provider::Cerebras => &CEREBRAS_REASONING,
            Provider::DeepSeek => &DEEPSEEK_REASONING,
            Provider::OpenRouter => &OPENROUTER_REASONING,
        }
    }

    /// OpenRouter считает стоимость сам и присылает её в `usage.cost` —
    /// прайс-лист под ним не нужен, а панель об этом честно предупреждает.
    pub fn cost_from_api(&self) -> bool {
        matches!(self, Provider::OpenRouter)
    }
}

/// Стратегия краткосрочной памяти: какую форму принимает текущий диалог в
/// запросе. Другие два слоя стратегии не касаются — у них свой тумблер.
/// Память фактов, которая в дне 10 была четвёртой стратегией, стала
/// отдельным слоем: она пережила и смену темы, и переключение стратегии, а
/// значит в один ряд со способами нарезать историю не вставала.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    /// Вся история как есть.
    #[default]
    Full,
    /// Пересказ начала плюс хвост из `keep_last` — поведение дня 9.
    Summary,
    /// Только последние `keep_last` сообщений, остальное отбрасывается.
    Window,
}

impl Strategy {
    pub const ALL: [Strategy; 3] = [Strategy::Full, Strategy::Summary, Strategy::Window];

    pub fn label(&self) -> &'static str {
        match self {
            Strategy::Full => "Полная история",
            Strategy::Summary => "Summary",
            Strategy::Window => "Скользящее окно",
        }
    }
}

/// Какие слои памяти подставляются в запрос этого чата. Краткосрочная в
/// список не входит: она включена всегда, а её форму задаёт стратегия.
/// Выключенный слой из файла не пропадает — он просто перестаёт уходить
/// провайдеру, и это самый прямой способ увидеть, на что он влиял.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layers {
    /// Профиль — не память, но включается и выключается так же: снять
    /// галочку и задать тот же вопрос — самый прямой способ увидеть, на что
    /// он влиял. Чаты из дня 11 поля не знают, им достаётся включённый.
    #[serde(default = "layer_on")]
    pub profile: bool,
    #[serde(default = "layer_on")]
    pub long_term: bool,
    #[serde(default = "layer_on")]
    pub working: bool,
}

fn layer_on() -> bool {
    true
}

impl Default for Layers {
    fn default() -> Self {
        Layers { profile: true, long_term: true, working: true }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Settings {
    /// `cerebras` | `deepseek` | `openrouter`.
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
    /// Сжимать ли контекст силами OpenRouter. На моделях с окном ≤ 8k он по
    /// умолчанию выбрасывает середину истории (middle-out), и переполнение
    /// становится невидимым: `prompt_tokens` замирает, модель забывает
    /// середину разговора. В Лаборатории это выключено — переполнение должно
    /// приходить ошибкой; включить обратно можно ради демонстрации. Старые
    /// чаты поля не знают, им достаётся `false`.
    #[serde(default)]
    pub router_compression: bool,
    /// Как собирается краткосрочная память запроса. Смена стратегии ничего из
    /// файла чата не удаляет: пересказ остаётся на месте, его просто перестают
    /// подставлять.
    pub strategy: Strategy,
    /// Какие из остальных слоёв памяти идут в запрос.
    #[serde(default)]
    pub layers: Layers,
    /// Сколько последних сообщений всегда идут в запрос как есть. Общее число
    /// для `summary` и `window`.
    pub keep_last: usize,
    /// Через сколько накопившихся сверх хвоста сообщений сворачивать снова.
    /// Только для `summary`.
    pub summarize_every: usize,
}

/// Настройки, как они лежат в файле чата. Отдельная форма нужна ради дня 9:
/// там стратегия называлась флагом `compress`, и такие чаты должны читаться
/// дальше — `compress: true` становится `summary`, всё остальное `full`.
#[derive(Deserialize)]
struct SettingsFile {
    provider: String,
    model: String,
    temperature: f64,
    reasoning: String,
    #[serde(default = "fallback_persona")]
    persona: String,
    system_prompt: String,
    #[serde(default)]
    router_compression: bool,
    #[serde(default)]
    strategy: Option<Strategy>,
    #[serde(default)]
    compress: Option<bool>,
    #[serde(default)]
    layers: Layers,
    #[serde(default = "default_keep_last")]
    keep_last: usize,
    #[serde(default = "default_summarize_every")]
    summarize_every: usize,
}

impl<'de> Deserialize<'de> for Settings {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Settings, D::Error> {
        let file = SettingsFile::deserialize(deserializer)?;
        let strategy = file.strategy.unwrap_or(match file.compress {
            Some(true) => Strategy::Summary,
            _ => Strategy::Full,
        });
        Ok(Settings {
            provider: file.provider,
            model: file.model,
            temperature: file.temperature,
            reasoning: file.reasoning,
            persona: file.persona,
            system_prompt: file.system_prompt,
            router_compression: file.router_compression,
            strategy,
            layers: file.layers,
            keep_last: file.keep_last,
            summarize_every: file.summarize_every,
        })
    }
}

fn fallback_persona() -> String {
    FALLBACK_PERSONA.to_string()
}

/// Умолчания сжатия. Чаты, сохранённые до его появления, полей не знают —
/// им достаются те же числа, что и новому чату.
const DEFAULT_KEEP_LAST: usize = 6;
const DEFAULT_SUMMARIZE_EVERY: usize = 10;
/// Границы разумного: один-два последних сообщения ещё держат разговор, а
/// полсотни хвоста или полсотни шага сводят сжатие на нет.
const KEEP_LAST_RANGE: std::ops::RangeInclusive<usize> = 1..=50;
const SUMMARIZE_EVERY_RANGE: std::ops::RangeInclusive<usize> = 2..=50;

fn default_keep_last() -> usize {
    DEFAULT_KEEP_LAST
}

fn default_summarize_every() -> usize {
    DEFAULT_SUMMARIZE_EVERY
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
            router_compression: false,
            strategy: Strategy::Full,
            layers: Layers::default(),
            keep_last: DEFAULT_KEEP_LAST,
            summarize_every: DEFAULT_SUMMARIZE_EVERY,
        }
    }
}

impl Settings {
    pub fn provider(&self) -> Result<Provider, String> {
        Provider::from_id(&self.provider).ok_or(format!("неизвестный провайдер: {}", self.provider))
    }

    pub fn model_info(&self) -> Option<&'static ModelInfo> {
        self.provider().ok()?.models().iter().find(|m| m.id == self.model)
    }

    /// Потолок ответа этой модели. Модели вне справочника быть не может
    /// (`validate` её не пропустит), но выдумывать панику здесь незачем.
    pub fn max_tokens(&self) -> u32 {
        self.model_info().map_or(DEFAULT_MAX_TOKENS, |m| m.max_tokens)
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
        if !KEEP_LAST_RANGE.contains(&self.keep_last) {
            return Err(format!(
                "«хранить последних» вне диапазона {}–{}",
                KEEP_LAST_RANGE.start(),
                KEEP_LAST_RANGE.end()
            ));
        }
        if !SUMMARIZE_EVERY_RANGE.contains(&self.summarize_every) {
            return Err(format!(
                "«сжимать каждые» вне диапазона {}–{}",
                SUMMARIZE_EVERY_RANGE.start(),
                SUMMARIZE_EVERY_RANGE.end()
            ));
        }
        Ok(())
    }
}

/// Реплика разговора. У ответа ассистента здесь же лежат его метрики: без
/// них после F5 нечем было бы нарисовать ни строку под ответом, ни график.
/// Старые файлы поля не знают — у них `None`. В запрос к провайдеру метрики
/// не уходят, туда собирается голая пара `role`/`content` (см. `wire`).
///
/// Роль `error` — отклонённый запрос: он остаётся в истории, но провайдеру
/// не отправляется (см. `goes_to_api`). У такой записи заполнены
/// `attempted_*`: сколько символов и примерно токенов было в отказанном
/// сообщении. У обычных реплик эти поля пустые и в файл не пишутся.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub metrics: Option<Metrics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempted_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempted_chars: Option<u32>,
}

impl Message {
    pub fn new(role: &str, content: String) -> Message {
        Message {
            role: role.to_string(),
            content,
            metrics: None,
            attempted_tokens: None,
            attempted_chars: None,
        }
    }
}

#[derive(Default, Debug, Clone, Copy, PartialEq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    /// Вход, попавший в кэш провайдера. У DeepSeek он почти бесплатен, у
    /// Cerebras такого поля нет — там всегда 0.
    pub cached_prompt_tokens: u64,
    /// Стоимость вызова в долларах, посчитанная самим провайдером.
    /// Присылает её только OpenRouter (`usage.cost`), у остальных `None` —
    /// там стоимость считается по прайс-листу из `ModelInfo`.
    pub api_cost_usd: Option<f64>,
}

/// Вклад одного слоя памяти в запрос: сколько записей он дал и во сколько
/// токенов примерно обошёлся. Токены — оценка по `chars_per_token` чата,
/// поэтому в интерфейсе рядом с ними стоит «≈».
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct LayerStat {
    pub items: usize,
    pub tokens: u64,
}

/// Разбивка запроса по слоям памяти. `None` — слой выключен в настройках
/// чата: это не то же самое, что пустой слой, и в строке под ответом они
/// показываются по-разному.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct LayerStats {
    #[serde(default)]
    pub profile: Option<LayerStat>,
    #[serde(default)]
    pub long_term: Option<LayerStat>,
    #[serde(default)]
    pub working: Option<LayerStat>,
    #[serde(default)]
    pub short_term: LayerStat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Что дал каждый слой памяти в этом запросе. Лежит в файле вместе с
    /// ответом: иначе после F5 строка под ответом теряла бы разбивку.
    #[serde(default)]
    pub layers: LayerStats,
    /// Оценка длины summary, если он был в запросе, — по `chars_per_token`
    /// чата. Точного числа тут нет и быть не может, поэтому «≈».
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_tokens_estimate: Option<u64>,
    /// Оценка `prompt_tokens`, если бы ушла вся история целиком. Заполняется
    /// только когда стратегия реально что-то изменила: пара «факт
    /// `prompt_tokens` / эта оценка» и есть «сэкономлено».
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_history_estimate: Option<u64>,
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
            // Своя цифра провайдера точнее прайс-листа: она уже учитывает и
            // скидки, и наценку роутера. Нет её — считаем по таблице.
            cost_usd: usage
                .api_cost_usd
                .or_else(|| settings.provider().ok().and_then(|p| cost(p, &settings.model, usage))),
            // Про слои знает не стрим, а `ask`: он собирал запрос и он же
            // проставляет эти поля перед тем, как положить метрики в историю.
            layers: LayerStats::default(),
            summary_tokens_estimate: None,
            full_history_estimate: None,
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
    /// История свёрнута: новый пересказ и что он стоил. Уходит до начала
    /// стрима, чтобы страница показала summary раньше ответа.
    Summary(SummaryInfo),
    /// Свернуть не вышло. Основной ответ от этого не отменяется — этот ход
    /// просто идёт с полной историей.
    SummaryError(String),
    /// Память обновлена: новая рабочая память, свежие предложения в
    /// долговременную и что стоил служебный вызов. Уходит после `done` —
    /// память собирается по уже готовому ответу.
    Memory(MemoryInfo),
    /// Обновить память не вышло. Ответ пользователю к этому моменту уже ушёл,
    /// оба слоя остаются прежними.
    MemoryError(String),
    Error(String),
}

/// Результат одной суммаризации — и в SSE, и в накопительные счётчики чата.
#[derive(Debug, Clone, Serialize)]
pub struct SummaryInfo {
    pub summary: String,
    pub covers: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cost: f64,
}

/// Что вернул служебный вызов памяти: дельта рабочей памяти и предложения в
/// долговременную. Сливает и сохраняет это вызывающий код (`apply_memory`) —
/// под своим замком и по свежепрочитанным с диска слоям: пока шёл запрос,
/// оба слоя мог поправить человек или соседний чат.
#[derive(Debug, Clone)]
pub struct MemoryReply {
    pub facts: Vec<Fact>,
    /// Предложенные заметки о человеке — они идут в профиль.
    pub profile: Vec<Fact>,
    pub suggestions: Vec<Suggestion>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cost: f64,
}

/// Результат одного обновления памяти — по той же форме, что и `SummaryInfo`.
/// Две корзины сразу: что легло в рабочую память само и что только предложено
/// в долговременную и ждёт кнопки человека.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryInfo {
    pub working: Vec<Fact>,
    /// Очередь предложений активного профиля и сколько их добавилось на этом
    /// ходу — по той же причине, что и у долговременной.
    pub profile_pending: Vec<Note>,
    pub profile_added: usize,
    pub pending: Vec<Entry>,
    /// Сколько предложений добавилось на этом ходу: дубли отсеиваются, и без
    /// этого числа непонятно, промолчала модель или её просто не послушали.
    pub added: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cost: f64,
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
        usage: usage.is_object().then(|| usage_from(usage)),
        completion_time: value["time_info"]["completion_time"].as_f64(),
        done: false,
        error: None,
    })
}

/// Счётчики из `usage`. Форма одна и у стрима, и у обычного ответа: полей,
/// которых у провайдера нет, просто не находится.
fn usage_from(usage: &Value) -> Usage {
    Usage {
        prompt_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0),
        completion_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
        reasoning_tokens: usage["completion_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or(0),
        cached_prompt_tokens: usage["prompt_cache_hit_tokens"].as_u64().unwrap_or(0),
        api_cost_usd: usage["cost"].as_f64(),
    }
}

/// Тело запроса. Общая часть — формат OpenAI Chat Completions; расходятся
/// провайдеры на потолке ответа, управлении рассуждением и на том, кто
/// присылает `usage` в стриме без спроса.
fn request_body(provider: Provider, settings: &Settings, messages: &[Message]) -> Value {
    let max_tokens = settings.max_tokens();
    let mut body = json!({
        "model": settings.model,
        "messages": wire(messages),
        "stream": true,
        "temperature": settings.temperature,
    });
    let map = body.as_object_mut().expect("собран как объект");
    match provider {
        Provider::Cerebras => {
            map.insert("max_completion_tokens".to_string(), json!(max_tokens));
            map.insert("reasoning_effort".to_string(), json!(settings.reasoning));
        }
        Provider::DeepSeek => {
            map.insert("max_tokens".to_string(), json!(max_tokens));
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
        Provider::OpenRouter => {
            map.insert("max_tokens".to_string(), json!(max_tokens));
            // Своя форма просьбы про usage: с ней в последнем чанке приезжает
            // и `cost` — сколько вызов стоил на самом деле.
            map.insert("usage".to_string(), json!({ "include": true }));
            // Плагин сжатия роутер включает сам на тесных моделях: молча
            // режет середину истории, вместо того чтобы вернуть 400.
            if !settings.router_compression {
                map.insert(
                    "plugins".to_string(),
                    json!([{ "id": "context-compression", "enabled": false }]),
                );
            }
        }
    }
    body
}

/// Реплики в том виде, в каком их ждёт API: только `role` и `content`.
/// Метрики — наша бухгалтерия, провайдеру их слать незачем.
///
/// Идущие подряд системные сообщения склеиваются в одно через пустую строку.
/// Внутри Лаборатории каждый слой — своё сообщение, и это правильно: так
/// считается разбивка по слоям и так видно, что откуда. Но шаблон чата у
/// Cerebras принимает ровно одно системное сообщение и на втором отвечает
/// «System message must be at the beginning»; DeepSeek и OpenRouter одно
/// сообщение принимают тоже. Склейка — на границе с API, модель памяти она не
/// трогает: текст блоков тот же, и `sent_chars` считается по нему же.
fn wire(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    for message in messages.iter().filter(|m| goes_to_api(m)) {
        let last_is_system = out.last().is_some_and(|m| m["role"] == "system");
        if message.role == "system" && last_is_system {
            let last = out.last_mut().expect("предыдущее сообщение есть");
            let merged = format!("{}\n\n{}", last["content"].as_str().unwrap_or(""), message.content);
            last["content"] = json!(merged);
            continue;
        }
        out.push(json!({ "role": message.role, "content": message.content }));
    }
    out
}

/// Уходит ли реплика провайдеру. Отклонённый запрос (`error`) живёт в истории
/// ради ленты и счётчиков, но роли `error` в API нет — такой запрос вернул бы
/// 400 сам по себе.
fn goes_to_api(m: &Message) -> bool {
    matches!(m.role.as_str(), "system" | "user" | "assistant")
}

/// Идёт ли в этот запрос summary вместо начала истории. Другая стратегия
/// пересказ из файла не трогает — просто не подставляет его.
fn uses_summary(chat: &Chat, settings: &Settings) -> bool {
    settings.strategy == Strategy::Summary
        && chat.summary.as_deref().is_some_and(|s| !s.trim().is_empty())
}

/// Служебное сообщение с пересказом. Роль `system`: это не чья-то реплика, а
/// заметка о том, что было раньше.
fn summary_message(summary: &str, covers: usize) -> Message {
    Message::new(
        "system",
        format!("Краткое содержание предыдущего разговора (сообщения 1–{covers}):\n{summary}"),
    )
}

/// Служебное сообщение профиля: как отвечать именно этому человеку. Роль
/// `system` и по той же причине, что у памяти, — это не реплика, а указание.
/// Пустые поля пропускаются, полностью пустой профиль блока не даёт: пустую
/// шапку модели слать незачем.
///
/// Шаги нумеруются здесь, а не хранятся с номерами: человек правит их
/// построчно, и вставка шага в середину не должна заставлять его
/// перенумеровывать список руками.
pub fn profile_message(profile: &Profile) -> Option<Message> {
    let mut lines: Vec<String> = Vec::new();
    let fields = [
        ("Обращение", &profile.address),
        ("Стиль", &profile.style),
        ("Формат", &profile.format),
        ("Ограничения", &profile.constraints),
        ("Контекст", &profile.context),
    ];
    for (label, value) in fields {
        let value = value.trim();
        if !value.is_empty() {
            lines.push(format!("{label}: {value}"));
        }
    }
    let steps = profile.step_lines();
    if !steps.is_empty() {
        lines.push("Порядок ответа:".to_string());
        lines.extend(steps.iter().enumerate().map(|(i, step)| format!("{}. {step}", i + 1)));
    }
    if !profile.notes.is_empty() {
        lines.push("Заметки о пользователе:".to_string());
        lines.extend(profile.notes.iter().map(|n| format!("- {}: {}", n.key, n.value)));
    }
    if lines.is_empty() {
        return None;
    }
    Some(Message::new("system", format!("Профиль пользователя:\n{}", lines.join("\n"))))
}

/// Сколько всего в профиле пунктов — заполненных полей, шагов и заметок.
/// Число идёт в разбивку под ответом: «профиль 9 пунктов ≈ 60 ток.».
pub fn profile_items(profile: &Profile) -> usize {
    let fields = [
        &profile.address,
        &profile.style,
        &profile.format,
        &profile.constraints,
        &profile.context,
    ];
    fields.iter().filter(|v| !v.trim().is_empty()).count()
        + profile.step_lines().len()
        + profile.notes.len()
}

/// Служебное сообщение долговременной памяти. Записи сгруппированы по типу:
/// модели полезно видеть, что про человека, что решено на все разговоры, а
/// что просто велено помнить. Роль `system` и по той же причине, что у
/// пересказа: это не чья-то реплика, а заметка. Пустой список блока не даёт.
pub fn long_term_message(entries: &[Entry]) -> Option<Message> {
    if entries.is_empty() {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();
    for kind in Kind::ALL {
        let mut group = entries.iter().filter(|e| e.kind == kind).peekable();
        if group.peek().is_none() {
            continue;
        }
        lines.push(format!("{}:", kind.group()));
        lines.extend(group.map(|e| format!("- {}: {}", e.key, e.value)));
    }
    Some(Message::new(
        "system",
        format!("Долговременная память (о пользователе и общие решения):\n{}", lines.join("\n")),
    ))
}

/// Служебное сообщение рабочей памяти — факты текущей задачи.
pub fn working_message(facts: &[Fact]) -> Option<Message> {
    if facts.is_empty() {
        return None;
    }
    let lines: Vec<String> = facts.iter().map(|f| format!("- {}: {}", f.key, f.value)).collect();
    Some(Message::new("system", format!("Рабочая память задачи:\n{}", lines.join("\n"))))
}

/// Краткосрочная память в том виде, который ей задала стратегия: служебный
/// пересказ (если он идёт) и отобранные реплики истории.
///
/// - `full` — вся история;
/// - `summary` — пересказ и всё непокрытое им (между суммаризациями хвост
///   длиннее `keep_last`, так и задумано);
/// - `window` — последние `keep_last` **элементов `messages`**: граница
///   считается по индексам массива, а фильтр `goes_to_api` работает уже после
///   неё, как и у покрытия пересказа. Поэтому отклонённый запрос место в окне
///   занимает, но провайдеру не уходит, и разделитель в ленте встаёт ровно по
///   этой границе.
///
/// Записи `error` отбрасываются здесь же: в API такой роли нет.
fn short_term(chat: &Chat, settings: &Settings) -> (Option<Message>, Vec<Message>) {
    let mut summary = None;
    let tail = match settings.strategy {
        Strategy::Full => chat.messages.as_slice(),
        Strategy::Summary if uses_summary(chat, settings) => {
            let covers = chat.summary_covers.min(chat.messages.len());
            summary =
                Some(summary_message(chat.summary.as_deref().unwrap_or(""), chat.summary_covers));
            &chat.messages[covers..]
        }
        // Сжатие выбрано, а пересказа ещё нет — история идёт целиком.
        Strategy::Summary => chat.messages.as_slice(),
        Strategy::Window => &chat.messages[chat.window_start(settings.keep_last)..],
    };
    (summary, tail.iter().filter(|m| goes_to_api(m)).cloned().collect())
}

/// Что уходит провайдеру — единственное место сборки запроса, по нему же
/// считаются `sent_chars` для калибровки. Порядок слоёв здесь и есть модель
/// памяти агента:
///
/// 1. системный промпт — личность;
/// 2. профиль: как отвечать этому человеку и в каком порядке;
/// 3. долговременная память: что решено на все разговоры и что велено помнить;
/// 4. рабочая память: факты текущей задачи;
/// 5. краткосрочная память: сам диалог в форме, которую задала стратегия.
///
/// Профиль стоит сразу за личностью и перед памятью: он говорит, **как**
/// отвечать, и это должно действовать на весь остальной контекст.
///
/// Выключенный слой просто не подставляется — на диске он остаётся, и
/// следующий запрос с галочкой вернёт его на место.
pub fn request_messages(
    chat: &Chat,
    settings: &Settings,
    system_prompt: &str,
    profile: &Profile,
    long_term: &[Entry],
    working: &[Fact],
) -> Vec<Message> {
    let mut out = vec![Message::new("system", system_prompt.to_string())];
    if settings.layers.profile {
        out.extend(profile_message(profile));
    }
    if settings.layers.long_term {
        out.extend(long_term_message(long_term));
    }
    if settings.layers.working {
        out.extend(working_message(working));
    }
    let (summary, tail) = short_term(chat, settings);
    out.extend(summary);
    out.extend(tail);
    out
}

/// Разбивка запроса по слоям — для строки под ответом. Считается по тем же
/// текстам, что собирает `request_messages`, поэтому числа в строке и в
/// запросе совпадают. Выключенный слой даёт `None`, а не нули: «выкл.» и
/// «пусто» — разные состояния.
pub fn layer_stats(
    chat: &Chat,
    settings: &Settings,
    profile: &Profile,
    long_term: &[Entry],
    working: &[Fact],
) -> LayerStats {
    let cpt = chat.chars_per_token_or_default();
    let stat = |items: usize, message: Option<Message>| LayerStat {
        items,
        tokens: message.map_or(0, |m| estimate_tokens(m.content.chars().count(), cpt)),
    };
    let (summary, tail) = short_term(chat, settings);
    let short_chars = summary.iter().chain(tail.iter()).map(|m| m.content.chars().count()).sum();
    LayerStats {
        profile: settings
            .layers
            .profile
            .then(|| stat(profile_items(profile), profile_message(profile))),
        long_term: settings
            .layers
            .long_term
            .then(|| stat(long_term.len(), long_term_message(long_term))),
        working: settings.layers.working.then(|| stat(working.len(), working_message(working))),
        short_term: LayerStat { items: tail.len(), tokens: estimate_tokens(short_chars, cpt) },
    }
}

/// Оценка длины в токенах: точного токенизатора у нас нет, есть калибровка
/// чата «символов на токен». Отсюда «≈» везде, где эти числа показываются.
fn estimate_tokens(chars: usize, chars_per_token: f64) -> u64 {
    (chars as f64 / chars_per_token).ceil() as u64
}

/// Сколько токенов ушло бы, отправь мы всю историю целиком: системный промпт
/// плюс все реплики, что уходят в API, по калибровке этого чата. Это и есть
/// «без сжатия было бы» — вторая половина пары, по которой считается экономия.
pub fn full_history_estimate(chat: &Chat, system_prompt: &str) -> u64 {
    let chars = system_prompt.chars().count() + sent_chars(&chat.messages);
    estimate_tokens(chars, chat.chars_per_token_or_default())
}

/// Промпт суммаризатора. Отдельный дешёвый вызов, как и у темы чата:
/// рассуждение выключено, стрима нет, потолок ответа небольшой.
const SUMMARY_PROMPT: &str = "Ты сжимаешь историю учебного диалога. Составь краткое, плотное содержание на русском: факты, числа, имена, договорённости, вопросы, которые остались открытыми, текущая тема. Без вступлений и без оценок. Не более 200 слов.";
const SUMMARY_MAX_TOKENS: u32 = 400;
/// Сколько символов одной реплики отдаём суммаризатору. Без обрезки «Длинный
/// текст» на 3 000 токенов, повторённый десять раз, переполнил бы уже сам
/// суммаризатор — тот самый случай, ради которого сжатие и делалось.
const SUMMARY_SOURCE_LIMIT: usize = 2000;

/// Вход суммаризатора: прежний пересказ (если был) и новые сообщения
/// человеческими строками. Записи `error` провайдеру не показываем и тут.
fn summary_source(previous: Option<&str>, slice: &[Message]) -> String {
    let lines: Vec<String> = slice
        .iter()
        .filter(|m| goes_to_api(m))
        .map(|m| {
            let who = if m.role == "user" { "Человек" } else { "Ассистент" };
            format!("{who}: {}", head(&m.content, SUMMARY_SOURCE_LIMIT))
        })
        .collect();
    match previous.map(str::trim).filter(|p| !p.is_empty()) {
        Some(previous) => {
            format!("Предыдущее содержание:\n{previous}\n\nНовые сообщения:\n{}", lines.join("\n"))
        }
        None => lines.join("\n"),
    }
}

fn summary_body(provider: Provider, model: &str, previous: Option<&str>, slice: &[Message]) -> Value {
    let mut body = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": SUMMARY_PROMPT },
            { "role": "user", "content": summary_source(previous, slice) },
        ],
        "stream": false,
        "temperature": 0.3,
    });
    let map = body.as_object_mut().expect("собран как объект");
    match provider {
        Provider::Cerebras => {
            map.insert("max_completion_tokens".to_string(), json!(SUMMARY_MAX_TOKENS));
            map.insert("reasoning_effort".to_string(), json!("none"));
        }
        Provider::DeepSeek => {
            map.insert("max_tokens".to_string(), json!(SUMMARY_MAX_TOKENS));
            map.insert("thinking".to_string(), json!({ "type": "disabled" }));
        }
        Provider::OpenRouter => {
            map.insert("max_tokens".to_string(), json!(SUMMARY_MAX_TOKENS));
            map.insert("usage".to_string(), json!({ "include": true }));
        }
    }
    body
}

/// Промпт хранителя памяти — и правило маршрутизации целиком. Тот же приём,
/// что у суммаризатора: отдельный дешёвый не-стримовый вызов, только просим не
/// прозу, а JSON — разобрать объект надёжнее, чем список строк. Рабочую память
/// просим дельтой: повторять весь список каждый ход модель не справляется, и
/// молча пропущенный факт означал бы его потерю.
///
/// Само правило — не «модель разберётся», а зашитый текст, продублированный в
/// README: в этом и смысл явной модели памяти. Половину решения при этом
/// принимает не модель: и `long_term`, и `profile` она только предлагает
/// (см. `add_pending` и `add_profile_pending`).
///
/// Корзины три: рабочая память задачи, заметки о самом человеке (они идут в
/// профиль — там и место нюансам конкретного пользователя) и долговременная
/// память общих решений и знаний.
const MEMORY_PROMPT: &str = "Ты ведёшь память агента и раскладываешь новое по трём корзинам. На входе рабочая память текущей задачи, ключи того, что уже известно про пользователя и лежит в долговременной памяти, и последняя пара реплик. Верни ТОЛЬКО JSON-объект вида {\"working\": {\"ключ\": \"значение\"}, \"profile\": [{\"key\": \"...\", \"value\": \"...\"}], \"long_term\": [{\"kind\": \"decision|knowledge\", \"key\": \"...\", \"value\": \"...\"}]}.
Куда что класть. working — всё, что относится только к текущей задаче или разговору: требования, цифры, выбранные варианты, открытые вопросы. profile — факты о самом человеке: роль, уровень, как ему отвечать, на чём он пишет, какие инструменты предпочитает. long_term с kind=decision — решения, которые человек явно назвал общими для всех проектов или разговоров. long_term с kind=knowledge — проверенные знания, которые человек попросил запомнить навсегда. Сомневаешься — клади в working.
В working верни только то, что появилось или изменилось в последней паре реплик; неизменившееся повторять не нужно; чтобы забыть факт, верни его ключ с пустой строкой. В profile и long_term не повторяй то, что уже есть в списках известного. Нового нет — верни {\"working\": {}, \"profile\": [], \"long_term\": []}. Не добавляй ничего, чего не было в диалоге. Ключи короткие, на русском, значения не длиннее 120 символов. Без пояснений и без markdown.";
const MEMORY_MAX_TOKENS: u32 = 500;
/// Сколько символов реплики отдаём хранителю памяти — по той же причине, что
/// и суммаризатору: «Длинный текст» переполнил бы уже его самого.
const MEMORY_SOURCE_LIMIT: usize = 2000;
/// Потолок рабочей памяти. Сорок фактов — это уже не выжимка задачи, а вторая
/// история.
const WORKING_LIMIT: usize = 40;

/// Текущая рабочая память в том виде, в каком модель должна вернуть дельту.
fn facts_json(facts: &[Fact]) -> String {
    let map: serde_json::Map<String, Value> =
        facts.iter().map(|f| (f.key.clone(), json!(f.value))).collect();
    Value::Object(map).to_string()
}

/// Вход хранителя памяти: рабочая память как есть, профиль и долговременная —
/// только ключами (у долговременной ещё и типами). Значения оттуда в этот
/// вызов не нужны, а дубли по ключу модель должна видеть, иначе она предложит
/// то же самое второй раз.
fn memory_source(
    working: &[Fact],
    profile: &Profile,
    long_term: &[Entry],
    user: &str,
    assistant: &str,
) -> String {
    let known_profile = if profile.notes.is_empty() {
        "нет".to_string()
    } else {
        profile.notes.iter().map(|n| format!("- {}", n.key)).collect::<Vec<_>>().join("\n")
    };
    let known = if long_term.is_empty() {
        "нет".to_string()
    } else {
        long_term
            .iter()
            .map(|e| format!("- {} / {}", e.kind.id(), e.key))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "Рабочая память:\n{}\n\nУже известно про пользователя:\n{known_profile}\n\nУже в долговременной памяти:\n{known}\n\nЧеловек: {}\n\nАссистент: {}",
        facts_json(working),
        head(user, MEMORY_SOURCE_LIMIT),
        head(assistant, MEMORY_SOURCE_LIMIT)
    )
}

fn memory_body(
    provider: Provider,
    model: &str,
    working: &[Fact],
    profile: &Profile,
    long_term: &[Entry],
    user: &str,
    assistant: &str,
) -> Value {
    let mut body = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": MEMORY_PROMPT },
            { "role": "user", "content": memory_source(working, profile, long_term, user, assistant) },
        ],
        "stream": false,
        "temperature": 0.2,
    });
    let map = body.as_object_mut().expect("собран как объект");
    match provider {
        Provider::Cerebras => {
            map.insert("max_completion_tokens".to_string(), json!(MEMORY_MAX_TOKENS));
            map.insert("reasoning_effort".to_string(), json!("none"));
        }
        Provider::DeepSeek => {
            map.insert("max_tokens".to_string(), json!(MEMORY_MAX_TOKENS));
            map.insert("thinking".to_string(), json!({ "type": "disabled" }));
        }
        Provider::OpenRouter => {
            map.insert("max_tokens".to_string(), json!(MEMORY_MAX_TOKENS));
            map.insert("usage".to_string(), json!({ "include": true }));
        }
    }
    body
}

/// Модель просили ответить голым JSON, но ```json … ``` она всё равно иногда
/// ставит.
fn strip_code_fence(raw: &str) -> &str {
    let text = raw.trim();
    let Some(rest) = text.strip_prefix("```") else { return text };
    // Первая строка после ``` — метка языка, её выбрасываем вместе с ней.
    let body = rest.split_once('\n').map_or("", |(_, body)| body);
    body.trim().trim_end_matches("```").trim()
}

/// Предложение в долговременную память. Не запись: id и время появятся, если
/// человек нажмёт «запомнить».
#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub kind: Kind,
    pub key: String,
    pub value: String,
}

/// Значение факта как текст. Модель иногда отвечает числом вместо строки —
/// приводим к тексту, а не выбрасываем факт.
fn as_text(value: &Value) -> String {
    match value.as_str() {
        Some(text) => text.trim().to_string(),
        None if value.is_null() => String::new(),
        None => value.to_string(),
    }
}

/// Объект `{"ключ": "значение"}` в список фактов. Пустые ключи и дубли по
/// ключу выбрасываются, пустое значение сохраняется: для слияния это признак
/// «забыть факт». Потолок памяти тут не применяется — список приходит дельтой,
/// режет его `merge_facts` уже после склейки.
fn facts_from(object: &serde_json::Map<String, Value>) -> Vec<Fact> {
    let mut facts: Vec<Fact> = Vec::new();
    for (key, value) in object {
        let key = key.trim();
        if key.is_empty() || facts.iter().any(|f| same_key(&f.key, key)) {
            continue;
        }
        facts.push(Fact { key: key.to_string(), value: as_text(value) });
    }
    facts
}

/// Разбор ответа хранителя памяти: три корзины сразу — рабочая память,
/// заметки профиля и долговременная. Всё, что не разобралось как объект, —
/// ошибка: лучше оставить слои прежними, чем испортить их. Элемент с
/// неизвестным `kind` или без ключа отбрасывается молча — один кривой элемент
/// не повод терять остальные. `kind: profile` из дня 11 в долговременную
/// больше не проходит: заметкам о человеке место в профиле.
fn parse_memory(raw: &str) -> Result<(Vec<Fact>, Vec<Fact>, Vec<Suggestion>), String> {
    let text = strip_code_fence(raw);
    let value: Value = serde_json::from_str(text)
        .map_err(|_| format!("память вернула не JSON: {}", truncate(text, 120)))?;
    let object = value.as_object().ok_or("память вернула не объект")?;

    let working = match object.get("working") {
        Some(working) if !working.is_null() => {
            facts_from(working.as_object().ok_or("working пришёл не объектом")?)
        }
        _ => Vec::new(),
    };

    // Заметки профиля приходят списком пар: типа у них нет, всё это про
    // одного человека. Разбираются той же меркой, что и долговременная:
    // пустой ключ, пустое значение и дубль по ключу — мимо.
    let mut profile: Vec<Fact> = Vec::new();
    for item in object.get("profile").and_then(|v| v.as_array()).unwrap_or(&Vec::new()) {
        let key = item["key"].as_str().unwrap_or_default().trim().to_string();
        let value = as_text(&item["value"]);
        if key.is_empty() || value.is_empty() || profile.iter().any(|f| same_key(&f.key, &key)) {
            continue;
        }
        profile.push(Fact { key, value });
    }

    let mut long_term: Vec<Suggestion> = Vec::new();
    for item in object.get("long_term").and_then(|v| v.as_array()).unwrap_or(&Vec::new()) {
        let Some(kind) = item["kind"].as_str().and_then(Kind::from_id) else { continue };
        let key = item["key"].as_str().unwrap_or_default().trim().to_string();
        let value = as_text(&item["value"]);
        if key.is_empty() || value.is_empty() {
            continue;
        }
        if long_term.iter().any(|s| s.kind == kind && same_key(&s.key, &key)) {
            continue;
        }
        long_term.push(Suggestion { kind, key, value });
    }
    Ok((working, profile, long_term))
}

/// Предложения модели в долговременную память сами туда не ложатся: они ждут
/// кнопки человека в `pending`. Дубли по (тип, ключ) — и с записанным, и с уже
/// ожидающим — отбрасываются, иначе очередь заполнилась бы одним и тем же за
/// три хода. Возвращает, сколько предложений реально добавилось.
fn add_pending(long_term: &mut LongTerm, suggestions: Vec<Suggestion>) -> usize {
    let mut added = 0;
    for suggestion in suggestions {
        if long_term.knows(suggestion.kind, &suggestion.key) {
            continue;
        }
        long_term.pending.push(Entry::new(suggestion.kind, suggestion.key, suggestion.value));
        added += 1;
    }
    added
}

/// То же самое для профиля: заметку о человеке модель предлагает, а
/// подтверждает человек. Дубли по ключу — и с заметками, и с уже
/// ожидающими — отбрасываются без учёта регистра.
fn add_profile_pending(profile: &mut Profile, suggestions: Vec<Fact>) -> usize {
    let mut added = 0;
    for suggestion in suggestions {
        if profile.knows(&suggestion.key) {
            continue;
        }
        profile.pending.push(Note::new(suggestion.key, suggestion.value));
        added += 1;
    }
    added
}

/// Положить ответ служебного вызова в слои. Зовётся из-под замка и по только
/// что прочитанным с диска слоям: между запросом к модели и этим моментом их
/// мог поправить человек или соседний чат, и слияние поверх устаревшей копии
/// затёрло бы его правку.
pub fn apply_memory(
    reply: MemoryReply,
    profile: &mut Profile,
    long_term: &mut LongTerm,
    working: &mut Working,
) -> MemoryInfo {
    merge_facts(&mut working.facts, reply.facts);
    let profile_added = add_profile_pending(profile, reply.profile);
    let added = add_pending(long_term, reply.suggestions);
    working.prompt_tokens += reply.prompt_tokens;
    working.completion_tokens += reply.completion_tokens;
    working.cost += reply.cost;
    MemoryInfo {
        working: working.facts.clone(),
        profile_pending: profile.pending.clone(),
        profile_added,
        pending: long_term.pending.clone(),
        added,
        prompt_tokens: reply.prompt_tokens,
        completion_tokens: reply.completion_tokens,
        cost: reply.cost,
    }
}

/// Склейка присланной дельты с рабочей памятью. Известный ключ обновляется на
/// месте — позиция в списке сохраняется, чтобы порядок фактов не прыгал от
/// хода к ходу; новый дописывается в конец; пустое значение забывает факт —
/// это единственный способ для модели что-то стереть. Ключей, которых в
/// ответе нет, слияние не касается: «не повторила» больше не значит «забудь».
/// Потолок применяется после склейки и режет только новые факты: выбрасывать
/// уже накопленное ради свежего — худший из двух вариантов потери.
fn merge_facts(existing: &mut Vec<Fact>, incoming: Vec<Fact>) {
    for fact in incoming {
        let known = existing.iter().position(|f| same_key(&f.key, &fact.key));
        match (known, fact.value.trim().is_empty()) {
            (Some(at), true) => {
                existing.remove(at);
            }
            (Some(at), false) => existing[at].value = fact.value,
            // Удаление того, чего и так нет, — не ошибка, просто ничего.
            (None, true) => {}
            (None, false) => {
                if existing.len() < WORKING_LIMIT {
                    existing.push(fact);
                }
            }
        }
    }
}

/// Последняя пара реплик: по ней и обновляется память. Нет ответа ассистента —
/// обновлять нечего.
fn last_exchange(chat: &Chat) -> Option<(String, String)> {
    let last = |role: &str| {
        chat.messages.iter().rev().find(|m| m.role == role).map(|m| m.content.clone())
    };
    Some((last("user")?, last("assistant")?))
}

/// Ответ не-стримового запроса: текст и его `usage`. Формат тот же, что у
/// стрима, только `message` вместо `delta`. Пустой текст — не пересказ:
/// возвращаем `None`, и ход просто пойдёт без сжатия.
fn parse_completion(value: &Value) -> Option<(String, Usage)> {
    let text = value["choices"][0]["message"]["content"].as_str()?.trim();
    (!text.is_empty()).then(|| (text.to_string(), usage_from(&value["usage"])))
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
        Provider::OpenRouter => {
            map.insert("max_tokens".to_string(), json!(TITLE_MAX_TOKENS));
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

/// Пометка «чат занят», снимающаяся сама. Занятость на чат, а не на агента:
/// два разных чата могут отвечать одновременно, один и тот же — нет.
pub struct Busy<'a> {
    agent: &'a Agent,
    chat: String,
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.agent.lock().remove(&self.chat);
    }
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
                "не задан ни один ключ (CEREBRAS_API_KEY, DEEPSEEK_API_KEY, OPENROUTER_API_KEY): скопируй .env.example в .env"
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

    /// Занять чат на один ход. Держать пометку нужно до конца служебного
    /// вызова памяти, а не до конца стрима: иначе следующий вопрос прочитал бы
    /// рабочую память раньше, чем в неё легло обновление предыдущего хода.
    /// Поэтому наружу отдаётся guard, а `ask` занятость сам не снимает.
    pub fn reserve(&self, chat: &str) -> Result<Busy<'_>, String> {
        if !self.lock().insert(chat.to_string()) {
            return Err("чат занят: дождись конца предыдущего ответа".to_string());
        }
        Ok(Busy { agent: self, chat: chat.to_string() })
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
        profile: &Profile,
        long_term: &LongTerm,
        working: &Working,
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

        chat.push_user(&text);

        // Порог проверяется после нового вопроса и до запроса: сворачивать
        // надо ровно ту историю, которая иначе уехала бы провайдеру целиком.
        // Осечка суммаризатора основной ответ не отменяет — этот ход просто
        // пойдёт с полной историей, а клиент увидит предупреждение.
        if chat.needs_summary(&chat.settings) {
            match self.summarize(provider, &key, chat).await {
                Ok(info) => {
                    chat.summary = Some(info.summary.clone());
                    chat.summary_covers = info.covers;
                    chat.summary_prompt_tokens += info.prompt_tokens;
                    chat.summary_completion_tokens += info.completion_tokens;
                    chat.summary_cost += info.cost;
                    let _ = tx.send(Event::Summary(info)).await;
                }
                Err(error) => {
                    let _ = tx.send(Event::SummaryError(error)).await;
                }
            }
        }

        let messages = request_messages(
            chat,
            &chat.settings,
            &chat.settings.system_prompt,
            profile,
            &long_term.entries,
            &working.facts,
        );
        // Символы считаем по тому, что реально уходит (вместе со всеми слоями):
        // по ним и по `prompt_tokens` из ответа калибруется «сколько символов в
        // токене» для оценки на клиенте.
        let sent_chars = sent_chars(&messages);
        let layers =
            layer_stats(chat, &chat.settings, profile, &long_term.entries, &working.facts);
        let strategy = chat.settings.strategy;
        let summarised = strategy == Strategy::Summary && uses_summary(chat, &chat.settings);
        let summary_estimate = summarised.then(|| {
            let (summary, _) = short_term(chat, &chat.settings);
            estimate_tokens(
                summary.map_or(0, |m| m.content.chars().count()),
                chat.chars_per_token_or_default(),
            )
        });
        // «Без стратегии было бы» считаем везде, где стратегия вообще что-то
        // меняет: у summary — только после первого сворачивания, у окна —
        // всегда, оно режет историю с первого же хода.
        let applied = match strategy {
            Strategy::Full => false,
            Strategy::Summary => summarised,
            Strategy::Window => true,
        };
        let full_estimate =
            applied.then(|| full_history_estimate(chat, &chat.settings.system_prompt));

        // Занятость чата снимает не `ask`, а guard из `reserve` у вызывающего
        // кода: держать её надо ещё и на время служебного вызова памяти.
        let outcome = self.stream(provider, &key, &chat.settings, &messages, tx).await;

        match outcome {
            Ok((answer, mut metrics)) => {
                metrics.layers = layers;
                metrics.summary_tokens_estimate = summary_estimate;
                metrics.full_history_estimate = full_estimate;
                chat.calibrate(sent_chars, metrics.prompt_tokens);
                chat.push_assistant(answer, Some(metrics.clone()));
                Ok(metrics)
            }
            Err(error) => {
                // Вопрос без ответа откатывается, но след от него остаётся:
                // иначе после переключения чата отказ исчезал бы, а счётчики
                // показывали бы последний удавшийся ход.
                chat.pop_user(&text);
                chat.push_error(&error, text.chars().count());
                Err(error)
            }
        }
    }

    /// Свернуть начало истории в пересказ — одним не-стримовым запросом к тому
    /// же провайдеру и той же модели, что и весь чат. Наружу отдаётся и
    /// `usage`: токены сжатия считаются отдельно от токенов разговора.
    ///
    /// Ошибку возвращаем текстом, а не глотаем: человек должен знать, что
    /// этот ход ушёл с полной историей.
    async fn summarize(
        &self,
        provider: Provider,
        key: &str,
        chat: &Chat,
    ) -> Result<SummaryInfo, String> {
        let keep_last = chat.settings.keep_last;
        let slice = chat.summary_slice(keep_last);
        if slice.is_empty() {
            return Err("сворачивать нечего".to_string());
        }
        let body = summary_body(provider, &chat.settings.model, chat.summary.as_deref(), slice);

        let response = self
            .client
            .post(provider.base_url())
            .bearer_auth(key)
            .json(&body)
            .send()
            .await
            .map_err(describe)?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(format!("API вернул {status}: {}", api_error(&text)));
        }
        let value: Value = response.json().await.map_err(describe)?;
        let (summary, usage) = parse_completion(&value).ok_or("суммаризатор вернул пустой ответ")?;

        Ok(SummaryInfo {
            summary,
            covers: chat.messages.len().saturating_sub(keep_last),
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            cost: usage
                .api_cost_usd
                .or_else(|| cost(provider, &chat.settings.model, usage))
                .unwrap_or(0.0),
        })
    }

    /// Обновить память по последней паре реплик — одним не-стримовым запросом к
    /// тому же провайдеру и той же модели. Зовётся после того, как ответ уже
    /// ушёл человеку: так в память попадают и решения модели, а стрим не ждёт
    /// лишнего вызова. Один ход — один такой запрос на оба слоя сразу.
    ///
    /// Рабочую память модель возвращает дельтой, и она **сливается** с прежним
    /// списком (`merge_facts`): пустой объект значит «без изменений», а не
    /// «очистить». Требовать повторения всей памяти каждый ход оказалось
    /// нереалистично: на живом прогоне дня 10 модель трижды недосчиталась части
    /// списка, и замена целиком эти факты стёрла.
    ///
    /// Долговременную память служебный вызов не пишет вовсе — только предлагает
    /// (`add_pending`). Это и есть «явно выбираем, что куда»: слой, который
    /// переживёт все следующие разговоры, наполняет человек, а не модель.
    ///
    /// Любая осечка — ошибка текстом и оба слоя на месте: терять память из-за
    /// одного кривого ответа незачем.
    ///
    /// Слои сюда приходят только на чтение — как вход промпта. Слить ответ и
    /// сохранить его обязан вызывающий код (`apply_memory`), по свежим копиям
    /// с диска и под своим замком.
    pub async fn update_memory(
        &self,
        chat: &Chat,
        profile: &Profile,
        long_term: &LongTerm,
        working: &Working,
    ) -> Result<MemoryReply, String> {
        let provider = chat.settings.provider()?;
        let key = self.key(provider)?.clone();
        let (user, assistant) = last_exchange(chat).ok_or("обновлять память не по чему")?;
        let body = memory_body(
            provider,
            &chat.settings.model,
            &working.facts,
            profile,
            &long_term.entries,
            &user,
            &assistant,
        );

        let response = self
            .client
            .post(provider.base_url())
            .bearer_auth(&key)
            .json(&body)
            .send()
            .await
            .map_err(describe)?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(format!("API вернул {status}: {}", api_error(&text)));
        }
        let value: Value = response.json().await.map_err(describe)?;
        let (text, usage) = parse_completion(&value).ok_or("память вернула пустой ответ")?;
        let (facts, profile, suggestions) = parse_memory(&text)?;

        Ok(MemoryReply {
            facts,
            profile,
            suggestions,
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            cost: usage
                .api_cost_usd
                .or_else(|| cost(provider, &chat.settings.model, usage))
                .unwrap_or(0.0),
        })
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
                Some("length") => format!("ответ оборван лимитом {} токенов", settings.max_tokens()),
                _ => "модель вернула пустой ответ".to_string(),
            });
        }

        let total_ms = started.elapsed().as_millis();
        let metrics = Metrics::build(settings, finish_reason, ttft_ms, total_ms, usage, completion_time);
        Ok((content, metrics))
    }
}

/// Сколько символов ушло в запрос: системный промпт плюс вся история.
/// Считаем `chars()`, а не байты: в кириллице байтов вдвое больше, и оценка
/// «символов на токен» по ним врала бы ровно во столько же раз. Отклонённые
/// запросы не в счёт: в тело запроса они не попадают, и калибровку по ним
/// вести значило бы завышать «символов на токен».
pub fn sent_chars(messages: &[Message]) -> usize {
    messages.iter().filter(|m| goes_to_api(m)).map(|m| m.content.chars().count()).sum()
}

/// Похожа ли ошибка провайдера на переполнение контекста. Точного признака
/// нет: код у всех 400, а текст свой у каждого — поэтому простая проверка на
/// несколько подстрок. Нужна она только для бейджа на пузыре, поэтому
/// ложное срабатывание дешевле пропуска.
pub fn is_context_overflow(error: &str) -> bool {
    let text = error.to_lowercase();
    text.contains("maximum context length")
        || text.contains("context_length_exceeded")
        || text.contains("too many tokens")
        || (text.contains("context") && (text.contains("exceed") || text.contains("limit")))
        || (text.contains("tokens") && (text.contains("exceed") || text.contains("limit")))
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
                cached_prompt_tokens: 0,
                api_cost_usd: None,
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

    /// Последний чанк OpenRouter: стоимость вызова провайдер считает сам.
    #[test]
    fn openrouter_final_chunk_carries_the_price_of_the_call() {
        let line = r#"data: {"id":"gen-1","choices":[{"delta":{"content":""},"finish_reason":"stop","index":0}],"model":"openai/gpt-3.5-turbo-0613","object":"chat.completion.chunk","usage":{"prompt_tokens":142,"completion_tokens":18,"total_tokens":160,"cost":0.000178,"completion_tokens_details":{"reasoning_tokens":0}}}"#;
        let usage = parse_chunk(line).expect("чанк OpenRouter разбирается").usage.expect("usage есть");
        assert_eq!(usage.prompt_tokens, 142);
        assert_eq!(usage.completion_tokens, 18);
        assert_eq!(usage.api_cost_usd, Some(0.000178));
        // Ни у Cerebras, ни у DeepSeek поля `cost` нет — там остаётся None.
        let cerebras = r#"data: {"choices":[{"delta":{},"index":0}],"usage":{"prompt_tokens":54,"completion_tokens":8}}"#;
        assert_eq!(parse_chunk(cerebras).unwrap().usage.unwrap().api_cost_usd, None);
    }

    #[test]
    fn openrouter_takes_the_cost_from_the_api_not_from_the_table() {
        let settings = Settings {
            provider: "openrouter".to_string(),
            model: "openai/gpt-3.5-turbo-0613".to_string(),
            reasoning: "none".to_string(),
            ..Settings::default()
        };
        assert!(settings.validate().is_ok());
        let usage = Usage {
            prompt_tokens: 142,
            completion_tokens: 18,
            reasoning_tokens: 0,
            cached_prompt_tokens: 0,
            // Своя цена роутера с наценкой: по прайс-листу вышло бы 0.000178.
            api_cost_usd: Some(0.000195),
        };
        let m = Metrics::build(&settings, Some("stop".to_string()), Some(300), 900, usage, None);
        assert_eq!(m.cost_usd, Some(0.000195), "цена берётся из usage.cost");
        let by_table = cost(Provider::OpenRouter, &settings.model, usage).expect("прайс есть");
        assert!((by_table - 0.000195).abs() > 1e-9, "таблица дала бы {by_table}");
        // Без цены от провайдера падаем обратно на таблицу.
        let fallback = Metrics::build(
            &settings,
            None,
            Some(300),
            900,
            Usage { api_cost_usd: None, ..usage },
            None,
        );
        assert_eq!(fallback.cost_usd, Some(by_table));
        assert!(Provider::OpenRouter.cost_from_api());
        assert!(!Provider::Cerebras.cost_from_api());
    }

    /// Потолок ответа входит в то же окно, что и промпт: у 4k-модели он свой.
    #[test]
    fn max_tokens_comes_from_the_model() {
        let messages = [Message::new("user", "привет".to_string())];
        let small = Settings {
            provider: "openrouter".to_string(),
            model: "openai/gpt-3.5-turbo-0613".to_string(),
            reasoning: "none".to_string(),
            ..Settings::default()
        };
        assert_eq!(small.max_tokens(), 1024);
        let body = request_body(Provider::OpenRouter, &small, &messages);
        assert_eq!(body["max_tokens"], json!(1024));
        assert_eq!(body["usage"]["include"], json!(true));
        assert!(body.get("reasoning").is_none(), "модели OpenRouter не рассуждают");
        assert!(body.get("reasoning_effort").is_none());

        let bigger = Settings { model: "gryphe/mythomax-l2-13b".to_string(), ..small };
        assert_eq!(bigger.max_tokens(), 2048);
        assert_eq!(request_body(Provider::OpenRouter, &bigger, &messages)["max_tokens"], json!(2048));

        // У остальных провайдеров потолок прежний.
        assert_eq!(Settings::default().max_tokens(), 4096);
        assert_eq!(Settings::default().model_info().map(|m| m.context_window), Some(65_536));
    }

    /// Метрики в историю попадают, а в запрос к провайдеру — нет.
    #[test]
    fn wire_messages_carry_only_role_and_content() {
        let mut answer = Message::new("assistant", "готово".to_string());
        answer.metrics = Some(Metrics::build(
            &Settings::default(),
            None,
            None,
            10,
            Usage::default(),
            None,
        ));
        let body = request_body(Provider::Cerebras, &Settings::default(), &[answer]);
        let sent = &body["messages"][0];
        assert_eq!(sent["content"], json!("готово"));
        assert!(sent.get("metrics").is_none(), "метрики провайдеру не отправляются");
        assert_eq!(sent.as_object().map(|o| o.len()), Some(2));
    }

    /// Слои собираются отдельными системными сообщениями, но шаблон чата
    /// Cerebras принимает только одно: на границе с API подряд идущие system
    /// склеиваются, а история остаётся как была.
    #[test]
    fn system_layers_are_glued_into_one_message_for_the_api() {
        let messages = [
            Message::new("system", "промпт".to_string()),
            Message::new("system", "Профиль пользователя:\nОбращение: на ты".to_string()),
            Message::new("system", "Рабочая память задачи:\n- бюджет: 400 тысяч".to_string()),
            Message::new("user", "вопрос".to_string()),
            Message::new("system", "Краткое содержание предыдущего разговора".to_string()),
            Message::new("assistant", "ответ".to_string()),
        ];
        let body = request_body(Provider::Cerebras, &Settings::default(), &messages);
        let sent = body["messages"].as_array().expect("список сообщений").clone();
        assert_eq!(sent.len(), 4, "три системных стали одним: {sent:?}");
        assert_eq!(
            sent[0]["content"],
            json!("промпт\n\nПрофиль пользователя:\nОбращение: на ты\n\nРабочая память задачи:\n- бюджет: 400 тысяч")
        );
        assert_eq!(sent[1]["role"], json!("user"));
        // Системное сообщение после реплики склеивать не с чем — оно своё.
        assert_eq!(sent[2]["role"], json!("system"));
        assert_eq!(sent[3]["role"], json!("assistant"));
    }

    /// Отклонённый запрос лежит в истории, но в API его роли нет: ни в теле
    /// запроса, ни в калибровке «символов на токен» он участвовать не должен.
    #[test]
    fn rejected_requests_stay_out_of_the_request_and_the_calibration() {
        let rejected = Message {
            attempted_tokens: Some(1400),
            attempted_chars: Some(4200),
            ..Message::new("error", "API вернул 400: context_length_exceeded".to_string())
        };
        let messages = [
            Message::new("system", "промпт".to_string()),
            Message::new("user", "вопрос".to_string()),
            Message::new("assistant", "ответ".to_string()),
            rejected,
            Message::new("user", "ещё вопрос".to_string()),
        ];

        let body = request_body(Provider::Cerebras, &Settings::default(), &messages);
        let sent = body["messages"].as_array().expect("массив сообщений");
        assert_eq!(sent.len(), 4, "запись error провайдеру не отправляется");
        assert!(sent.iter().all(|m| m["role"] != json!("error")), "{sent:?}");
        assert_eq!(sent.last().map(|m| m["content"].clone()), Some(json!("ещё вопрос")));

        // 6 + 6 + 5 + 10 — текст ошибки в калибровку не входит.
        assert_eq!(sent_chars(&messages), 27);
    }

    #[test]
    fn overflow_is_told_apart_from_other_failures() {
        for text in [
            "API вернул 400 Bad Request: This model's maximum context length is 4095 tokens, however you requested 5200 tokens",
            "API вернул 400: {\"code\":\"context_length_exceeded\"}",
            "Input too many tokens for this model",
            "prompt tokens exceed the limit of the model",
        ] {
            assert!(is_context_overflow(text), "должно считаться переполнением: {text}");
        }
        for text in [
            "не удалось соединиться с API (сеть, DNS или TLS)",
            "API вернул 401 Unauthorized: no auth credentials found",
            "модель вернула пустой ответ",
        ] {
            assert!(!is_context_overflow(text), "не переполнение: {text}");
        }
    }

    #[test]
    fn sent_chars_counts_characters_not_bytes() {
        let messages = [
            Message::new("system", "промпт".to_string()),
            Message::new("user", "вопрос".to_string()),
        ];
        assert_eq!(sent_chars(&messages), 12);
        assert_eq!(sent_chars(&[]), 0);
    }

    #[test]
    fn done_marker_is_recognised() {
        let done = parse_chunk("data: [DONE]").expect("маркер конца разбирается");
        assert!(done.done);
        assert_eq!(done, Chunk { done: true, ..Chunk::default() });
    }

    #[test]
    fn cost_follows_the_price_table_and_is_none_without_prices() {
        let usage = Usage { prompt_tokens: 54, completion_tokens: 312, reasoning_tokens: 8, cached_prompt_tokens: 0, api_cost_usd: None };
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
            api_cost_usd: None,
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
            Usage { prompt_tokens: 10, completion_tokens: 100, reasoning_tokens: 0, cached_prompt_tokens: 0, api_cost_usd: None },
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
        let messages = [Message::new("user", "привет".to_string())];

        let cerebras = request_body(Provider::Cerebras, &Settings::default(), &messages);
        assert_eq!(cerebras["max_completion_tokens"], json!(4096));
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
        assert_eq!(body["max_tokens"], json!(4096));
        assert!(body.get("max_completion_tokens").is_none());
        assert_eq!(body["stream_options"]["include_usage"], json!(true));
        assert_eq!(body["thinking"]["type"], json!("disabled"));
        assert!(body.get("reasoning_effort").is_none(), "выключенное рассуждение уровня не имеет");

        let on = Settings { reasoning: "high".to_string(), ..off };
        let body = request_body(Provider::DeepSeek, &on, &messages);
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert_eq!(body["reasoning_effort"], json!("high"));
    }

    /// Сжатие контекста роутером — единственный способ увидеть переполнение
    /// как ошибку, поэтому по умолчанию плагин выключается явно.
    #[test]
    fn openrouter_turns_off_context_compression_unless_asked() {
        let messages = [Message::new("user", "привет".to_string())];
        let router = Settings {
            provider: "openrouter".to_string(),
            model: "openai/gpt-3.5-turbo-0613".to_string(),
            reasoning: "none".to_string(),
            ..Settings::default()
        };
        assert!(!router.router_compression, "по умолчанию сжатие выключено");

        let body = request_body(Provider::OpenRouter, &router, &messages);
        assert_eq!(body["plugins"][0]["id"], json!("context-compression"));
        assert_eq!(body["plugins"][0]["enabled"], json!(false));

        // Включённое сжатие — это поведение роутера по умолчанию: поля нет.
        let on = Settings { router_compression: true, ..router.clone() };
        assert!(request_body(Provider::OpenRouter, &on, &messages).get("plugins").is_none());

        // У остальных провайдеров плагинов нет ни при каком значении флага.
        for compression in [false, true] {
            let cerebras = Settings { router_compression: compression, ..Settings::default() };
            assert!(request_body(Provider::Cerebras, &cerebras, &messages).get("plugins").is_none());
            let deepseek = Settings {
                provider: "deepseek".to_string(),
                model: "deepseek-v4-flash".to_string(),
                router_compression: compression,
                ..Settings::default()
            };
            assert!(request_body(Provider::DeepSeek, &deepseek, &messages).get("plugins").is_none());
        }
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
        assert!(!settings.router_compression, "старый чат сжатия роутером не просил");
        assert_eq!(settings.system_prompt, "старый промпт", "свой промпт чата не трогаем");
        assert!(settings.validate().is_ok());
    }

    /// История с отклонённым запросом посередине: он лежит в файле, но ни в
    /// запрос, ни суммаризатору не уходит.
    fn chat_with_history() -> Chat {
        let mut chat = Chat::new(Settings::default());
        chat.messages = vec![
            Message::new("user", "первый".to_string()),
            Message::new("assistant", "ответ один".to_string()),
            Message::new("user", "второй".to_string()),
            Message::new("error", "API вернул 400".to_string()),
            Message::new("assistant", "ответ два".to_string()),
            Message::new("user", "третий".to_string()),
        ];
        chat
    }

    /// Короткая долговременная память для тестов сборки запроса.
    fn entry(kind: Kind, key: &str, value: &str) -> Entry {
        Entry::new(kind, key.to_string(), value.to_string())
    }

    #[test]
    fn the_full_strategy_sends_the_whole_history() {
        let chat = chat_with_history();
        let settings = Settings::default();
        assert_eq!(settings.strategy, Strategy::Full, "новый чат ничего не режет");

        let messages = request_messages(&chat, &settings, "промпт", &Profile::default(), &[], &[]);
        assert_eq!(messages.len(), 6, "системный промпт и пять реплик из шести");
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[0].content, "промпт");
        assert_eq!(messages[1].content, "первый");
        assert!(messages.iter().all(|m| m.role != "error"), "запись отказа провайдеру не уходит");
        assert_eq!(messages.last().map(|m| m.content.as_str()), Some("третий"));

        // Сжатие выбрано, а пересказа ещё нет — история идёт целиком.
        let waiting = Settings { strategy: Strategy::Summary, ..Settings::default() };
        assert_eq!(request_messages(&chat, &waiting, "промпт", &Profile::default(), &[], &[]).len(), 6);
        let empty = Chat { summary: Some("   ".to_string()), summary_covers: 3, ..chat.clone() };
        assert_eq!(
            request_messages(&empty, &waiting, "промпт", &Profile::default(), &[], &[]).len(),
            6,
            "пустой пересказ не в счёт"
        );
    }

    /// Профиль для сборки запроса: непустые поля, шаги с мусорными строками
    /// и одна заметка.
    fn test_profile() -> Profile {
        Profile {
            address: "на ты, по имени Антон".to_string(),
            style: "кратко, только суть".to_string(),
            steps: "Сразу решение\n\n   Риски   \n".to_string(),
            notes: vec![Note::new("язык".to_string(), "Rust".to_string())],
            ..Profile::new("Инженер".to_string())
        }
    }

    /// Порядок слоёв в запросе — и есть модель памяти: личность, как отвечать
    /// этому человеку, что решено на все разговоры, что за задача, сам диалог.
    #[test]
    fn the_layers_go_into_the_request_in_order() {
        let chat = chat_with_history();
        let settings = Settings { keep_last: 2, ..Settings::default() };
        let profile = test_profile();
        let long_term = [
            entry(Kind::Decision, "язык ответов", "русский"),
            entry(Kind::Knowledge, "порог датчика", "17 см"),
            entry(Kind::Decision, "платформа", "дифференциальная"),
        ];
        let working = [fact("бюджет", "400 тысяч"), fact("срок", "3 месяца")];

        let messages =
            request_messages(&chat, &settings, "промпт", &profile, &long_term, &working);
        assert_eq!(messages.len(), 9, "промпт, три слоя и пять реплик: {messages:?}");
        assert_eq!(messages[0].content, "промпт");
        // Профиль стоит сразу за личностью и перед памятью: он говорит, как
        // отвечать, и это должно действовать на весь остальной контекст.
        assert_eq!(messages[1].role, "system", "профиль — не чья-то реплика");
        assert_eq!(
            messages[1].content,
            "Профиль пользователя:\n\
             Обращение: на ты, по имени Антон\n\
             Стиль: кратко, только суть\n\
             Порядок ответа:\n1. Сразу решение\n2. Риски\n\
             Заметки о пользователе:\n- язык: Rust"
        );
        assert_eq!(messages[2].role, "system", "долговременная — тоже не реплика");
        // Записи одного типа стоят вместе, типы — в порядке Kind::ALL.
        assert_eq!(
            messages[2].content,
            "Долговременная память (о пользователе и общие решения):\n\
             Решения:\n- язык ответов: русский\n- платформа: дифференциальная\n\
             Знания:\n- порог датчика: 17 см"
        );
        assert_eq!(
            messages[3].content,
            "Рабочая память задачи:\n- бюджет: 400 тысяч\n- срок: 3 месяца"
        );
        assert_eq!(messages[4].content, "первый", "дальше идёт сам диалог");
        assert_eq!(messages.last().map(|m| m.content.as_str()), Some("третий"));
    }

    /// Пустой профиль блока не даёт: пустую шапку модели слать незачем.
    #[test]
    fn an_empty_profile_adds_nothing_to_the_request() {
        assert!(profile_message(&Profile::default()).is_none());
        // Название профиля в запрос не идёт — оно только для интерфейса.
        assert!(profile_message(&Profile::new("Первокурсник".to_string())).is_none());
        assert_eq!(profile_items(&Profile::default()), 0);
        // Пункты — заполненные поля, шаги и заметки: два поля, два шага,
        // одна заметка.
        assert_eq!(profile_items(&test_profile()), 5);

        let chat = chat_with_history();
        let messages = request_messages(
            &chat,
            &Settings::default(),
            "промпт",
            &Profile::new("Первокурсник".to_string()),
            &[],
            &[],
        );
        assert_eq!(messages.len(), 6, "остались промпт и история");
    }

    /// Выключенный слой не уходит провайдеру, но с диска не пропадает — на
    /// этом и держится проверка «как слой влияет на ответы».
    #[test]
    fn a_switched_off_layer_disappears_from_the_request_only() {
        let chat = chat_with_history();
        let profile = test_profile();
        let long_term = [entry(Kind::Decision, "язык ответов", "русский")];
        let working = [fact("бюджет", "400 тысяч")];

        let without_profile = Settings {
            layers: Layers { profile: false, long_term: true, working: true },
            ..Settings::default()
        };
        let messages =
            request_messages(&chat, &without_profile, "промпт", &profile, &long_term, &working);
        assert!(messages.iter().all(|m| !m.content.starts_with("Профиль пользователя")));
        assert!(messages[1].content.starts_with("Долговременная память"));
        assert_eq!(profile.notes.len(), 1, "выключение слоя ничего не стирает");

        let without_long = Settings {
            layers: Layers { profile: true, long_term: false, working: true },
            ..Settings::default()
        };
        let messages =
            request_messages(&chat, &without_long, "промпт", &profile, &long_term, &working);
        assert!(messages.iter().all(|m| !m.content.starts_with("Долговременная память")));
        assert!(messages[1].content.starts_with("Профиль пользователя"));
        assert_eq!(messages[2].content, "Рабочая память задачи:\n- бюджет: 400 тысяч");
        assert_eq!(long_term.len(), 1, "выключение слоя ничего не стирает");

        let without_working = Settings {
            layers: Layers { profile: true, long_term: true, working: false },
            ..Settings::default()
        };
        let messages =
            request_messages(&chat, &without_working, "промпт", &profile, &long_term, &working);
        assert!(messages[2].content.starts_with("Долговременная память"));
        assert!(messages.iter().all(|m| !m.content.starts_with("Рабочая память")));

        let neither = Settings {
            layers: Layers { profile: false, long_term: false, working: false },
            ..Settings::default()
        };
        let messages =
            request_messages(&chat, &neither, "промпт", &profile, &long_term, &working);
        assert_eq!(messages.len(), 6, "остались промпт и история");
        assert_eq!(messages[1].content, "первый");
    }

    /// Разбивка под ответом считается по тем же текстам, что ушли в запрос:
    /// иначе строка врала бы ровно там, где её и читают.
    #[test]
    fn layer_stats_count_what_actually_went_out() {
        let mut chat = chat_with_history();
        chat.calibrate(400, 100); // 4 символа на токен
        let settings = Settings { strategy: Strategy::Window, keep_last: 3, ..Settings::default() };
        let profile = Profile {
            address: "на ты".to_string(),
            ..Profile::new("Инженер".to_string())
        };
        let long_term = [entry(Kind::Decision, "роль", "студент")];
        let working = [fact("бюджет", "400 тысяч")];

        // Ожидания — посчитанные руками числа, а не те же функции ещё раз.
        // «Профиль пользователя:\nОбращение: на ты» — 38 символов, при
        // четырёх символах на токен это 10.
        let stats = layer_stats(&chat, &settings, &profile, &long_term, &working);
        assert_eq!(stats.profile, Some(LayerStat { items: 1, tokens: 10 }));
        // «Долговременная память (о пользователе и общие решения):\nРешения:\n
        // - роль: студент» — 80 символов, это 20 токенов.
        assert_eq!(stats.long_term, Some(LayerStat { items: 1, tokens: 20 }));
        // «Рабочая память задачи:\n- бюджет: 400 тысяч» — 42 символа.
        assert_eq!(stats.working, Some(LayerStat { items: 1, tokens: 11 }));
        // Последние три элемента — error, «ответ два», «третий»: запись отказа
        // место в окне занимает, но в слой не считается. 15 символов на 4.
        assert_eq!(stats.short_term, LayerStat { items: 2, tokens: 4 });

        // Выключенный слой даёт None, а не нули: «выкл.» и «пусто» — разное.
        let off = Settings {
            layers: Layers { profile: false, long_term: false, working: true },
            ..settings.clone()
        };
        let stats = layer_stats(&chat, &off, &profile, &long_term, &working);
        assert_eq!(stats.profile, None);
        assert_eq!(stats.long_term, None);
        assert_eq!(stats.working.map(|s| s.items), Some(1));

        // Включённый пустой слой — ноль записей и ноль токенов.
        let stats = layer_stats(&chat, &settings, &Profile::default(), &[], &[]);
        assert_eq!(stats.profile, Some(LayerStat { items: 0, tokens: 0 }));
        assert_eq!(stats.long_term, Some(LayerStat { items: 0, tokens: 0 }));
        assert_eq!(stats.working, Some(LayerStat { items: 0, tokens: 0 }));
    }

    #[test]
    fn a_summary_replaces_the_covered_head_of_the_history() {
        let chat = Chat {
            summary: Some("Человек назвал робота Кузей, порог датчика 17 см.".to_string()),
            summary_covers: 3,
            ..chat_with_history()
        };
        let settings =
            Settings { strategy: Strategy::Summary, keep_last: 2, ..Settings::default() };

        let messages = request_messages(&chat, &settings, "промпт", &Profile::default(), &[], &[]);
        assert_eq!(messages.len(), 4, "промпт, пересказ и два непокрытых сообщения");
        assert_eq!(messages[0].content, "промпт");
        assert_eq!(messages[1].role, "system", "пересказ — не чья-то реплика");
        assert!(messages[1].content.starts_with("Краткое содержание предыдущего разговора (сообщения 1–3):"));
        assert!(messages[1].content.contains("Кузей"));
        // Покрытая половина осталась в истории, но в запрос не пошла.
        assert!(messages.iter().all(|m| m.content != "первый"));
        assert_eq!(messages[2].content, "ответ два");
        assert_eq!(messages[3].content, "третий");
        assert!(messages.iter().all(|m| m.role != "error"));
        assert_eq!(chat.messages.len(), 6, "сжатие историю на диске не трогает");

        // Пересказ — часть краткосрочного слоя, поэтому встаёт после слоёв
        // памяти, а не перед ними.
        let layered = request_messages(
            &chat,
            &settings,
            "промпт",
            &test_profile(),
            &[entry(Kind::Decision, "язык ответов", "русский")],
            &[fact("бюджет", "400 тысяч")],
        );
        assert!(layered[1].content.starts_with("Профиль пользователя"));
        assert!(layered[2].content.starts_with("Долговременная память"));
        assert!(layered[3].content.starts_with("Рабочая память"));
        assert!(layered[4].content.starts_with("Краткое содержание"));

        // Сменили стратегию — пересказ остался в файле, но в запрос не идёт.
        let off = Settings { strategy: Strategy::Full, ..settings };
        assert_eq!(request_messages(&chat, &off, "промпт", &Profile::default(), &[], &[]).len(), 6);
        assert!(chat.summary.is_some());
    }

    #[test]
    fn the_full_history_estimate_divides_chars_by_the_calibration() {
        let mut chat = Chat::new(Settings::default());
        chat.messages = vec![
            Message::new("user", "абвг".to_string()),
            Message::new("assistant", "дежз".to_string()),
            Message::new("error", "ошибка на восемь".to_string()),
        ];
        // 8 символов промпта плюс 4 + 4 реплик; текст отказа не в счёт.
        assert_eq!(chat.chars_per_token_or_default(), 3.0, "до первого ответа — умолчание");
        assert_eq!(full_history_estimate(&chat, "промптик"), 6, "16 / 3 с округлением вверх");

        chat.calibrate(400, 100); // 4 символа на токен
        assert_eq!(full_history_estimate(&chat, "промптик"), 4);
        assert_eq!(estimate_tokens(0, 4.0), 0);
    }

    #[test]
    fn the_summary_request_is_short_and_is_not_streamed() {
        let long = "я".repeat(3000);
        let slice = [
            Message::new("user", long.clone()),
            Message::new("error", "API вернул 400".to_string()),
            Message::new("assistant", "коротко".to_string()),
        ];

        let body = summary_body(Provider::DeepSeek, "deepseek-v4-flash", None, &slice);
        assert_eq!(body["stream"], json!(false));
        assert_eq!(body["temperature"], json!(0.3));
        assert_eq!(body["max_tokens"], json!(400));
        assert_eq!(body["thinking"]["type"], json!("disabled"), "рассуждение суммаризатору не нужно");
        assert_eq!(body["messages"][0]["content"], json!(SUMMARY_PROMPT));

        let user = body["messages"][1]["content"].as_str().expect("вторая реплика — строка");
        assert_eq!(user.matches('я').count(), SUMMARY_SOURCE_LIMIT, "длинная реплика обрезана");
        assert!(user.starts_with("Человек: "));
        assert!(user.contains("Ассистент: коротко"));
        assert!(!user.contains("API вернул 400"), "отклонённый запрос суммаризатору не показываем");
        assert!(!user.contains("Предыдущее содержание"), "сворачиваем впервые");

        // Второй заход отдаёт модели прежний пересказ вместе с новыми репликами.
        let again = summary_body(Provider::Cerebras, "qwen-3.8-27b", Some("было раньше"), &slice);
        assert_eq!(again["max_completion_tokens"], json!(400));
        assert_eq!(again["reasoning_effort"], json!("none"));
        let user = again["messages"][1]["content"].as_str().unwrap();
        assert!(user.starts_with("Предыдущее содержание:\nбыло раньше\n\nНовые сообщения:\n"));
    }

    #[test]
    fn a_summary_reply_is_parsed_with_its_usage() {
        let body = r#"{"id":"c-1","object":"chat.completion","model":"deepseek-v4-flash",
            "choices":[{"index":0,"message":{"role":"assistant","content":"  Робота зовут Кузя, порог 17 см.  "},
            "finish_reason":"stop"}],
            "usage":{"prompt_tokens":820,"completion_tokens":96,"total_tokens":916,
            "prompt_cache_hit_tokens":640}}"#;
        let value: Value = serde_json::from_str(body).expect("ответ разбирается");
        let (summary, usage) = parse_completion(&value).expect("пересказ есть");
        assert_eq!(summary, "Робота зовут Кузя, порог 17 см.");
        assert_eq!(usage.prompt_tokens, 820);
        assert_eq!(usage.completion_tokens, 96);
        assert_eq!(usage.cached_prompt_tokens, 640);

        // Пустой ответ пересказом не считается: ход просто пойдёт без сжатия.
        let empty: Value =
            serde_json::from_str(r#"{"choices":[{"message":{"content":"   "}}]}"#).unwrap();
        assert!(parse_completion(&empty).is_none());
        let broken: Value = serde_json::from_str(r#"{"error":{"message":"нет ключа"}}"#).unwrap();
        assert!(parse_completion(&broken).is_none());
    }

    #[test]
    fn compression_limits_are_validated() {
        let ok = Settings {
            strategy: Strategy::Summary,
            keep_last: 6,
            summarize_every: 10,
            ..Settings::default()
        };
        assert!(ok.validate().is_ok());

        for keep_last in [1, 50] {
            assert!(Settings { keep_last, ..ok.clone() }.validate().is_ok(), "{keep_last} допустим");
        }
        for keep_last in [0, 51] {
            assert!(Settings { keep_last, ..ok.clone() }.validate().is_err(), "{keep_last} вне границ");
        }
        for every in [2, 50] {
            assert!(Settings { summarize_every: every, ..ok.clone() }.validate().is_ok());
        }
        for every in [0, 1, 51] {
            assert!(Settings { summarize_every: every, ..ok.clone() }.validate().is_err());
        }
    }

    /// Окно режет историю по индексам массива: отклонённый запрос место в нём
    /// занимает, но провайдеру, как и раньше, не уходит.
    #[test]
    fn the_window_sends_only_the_last_messages() {
        let chat = chat_with_history();
        let settings =
            Settings { strategy: Strategy::Window, keep_last: 3, ..Settings::default() };

        let messages = request_messages(&chat, &settings, "промпт", &Profile::default(), &[], &[]);
        assert_eq!(messages[0].role, "system", "системный промпт всегда первый");
        assert_eq!(messages[0].content, "промпт");
        // Последние три элемента истории — error, «ответ два», «третий»;
        // запись отказа отсеивается уже после границы окна.
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1].content, "ответ два");
        assert_eq!(messages[2].content, "третий");
        assert!(messages.iter().all(|m| m.role != "error"));
        assert_eq!(chat.messages.len(), 6, "окно историю на диске не трогает");

        // Окно длиннее истории отправляет её целиком.
        let wide = Settings { keep_last: 50, ..settings.clone() };
        assert_eq!(request_messages(&chat, &wide, "промпт", &Profile::default(), &[], &[]).len(), 6);

        // Окно режет только краткосрочный слой: рабочая память за границу не
        // уезжает, в этом и смысл отдельного слоя.
        let working = [fact("бюджет", "400 тысяч")];
        let messages = request_messages(&chat, &settings, "промпт", &Profile::default(), &[], &working);
        assert_eq!(messages[1].content, "Рабочая память задачи:\n- бюджет: 400 тысяч");
        assert_eq!(messages.len(), 4);

        // Пересказ при этой стратегии не подставляется, даже если он есть.
        let with_summary = Chat {
            summary: Some("было раньше".to_string()),
            summary_covers: 3,
            ..chat.clone()
        };
        let messages = request_messages(&with_summary, &settings, "промпт", &Profile::default(), &[], &[]);
        assert!(messages.iter().all(|m| !m.content.contains("было раньше")));
    }

    /// Чаты дня 9 знали не стратегию, а флаг сжатия.
    #[test]
    fn the_day_nine_compress_flag_becomes_a_strategy() {
        let with = |field: &str| {
            format!(
                r#"{{"provider":"cerebras","model":"qwen-3.8-27b","temperature":0.7,
                "reasoning":"none","persona":"free","system_prompt":"промпт"{field}}}"#
            )
        };
        let parse = |json: String| serde_json::from_str::<Settings>(&json);

        assert_eq!(parse(with(r#","compress":true"#)).unwrap().strategy, Strategy::Summary);
        assert_eq!(parse(with(r#","compress":false"#)).unwrap().strategy, Strategy::Full);
        assert_eq!(parse(with("")).unwrap().strategy, Strategy::Full, "поля нет вовсе");
        // Новое поле старое перебивает: в файле дня 10 их вместе не бывает, но
        // приоритет должен быть однозначным.
        let both = parse(with(r#","compress":true,"strategy":"window""#)).unwrap();
        assert_eq!(both.strategy, Strategy::Window);
        assert_eq!(both.keep_last, 6, "умолчания хвоста и шага прежние");
        assert_eq!(both.summarize_every, 10);
        assert!(both.validate().is_ok());

        // Стратегия, которой нет, — отказ разбора, а не молчаливый `full`.
        assert!(parse(with(r#","strategy":"телепатия""#)).is_err());

        // В файл пишется уже только стратегия.
        let text = serde_json::to_string(&Settings::default()).expect("сериализуется");
        assert!(text.contains(r#""strategy":"full""#), "{text}");
        assert!(!text.contains("compress\""), "{text}");
    }

    #[test]
    fn the_memory_request_is_short_and_is_not_streamed() {
        // Буквы «ъ» нет ни в шапке промпта, ни в значениях — по ней ниже и
        // считается обрезка длинной реплики.
        let long = "ъ".repeat(3000);
        let working = [fact("бюджет", "400 тыс. рублей")];
        let profile = Profile {
            notes: vec![Note::new("роль".to_string(), "студент-робототехник".to_string())],
            ..Profile::new("Первокурсник".to_string())
        };
        let long_term = [entry(Kind::Decision, "язык ответов", "русский")];

        let body = memory_body(
            Provider::DeepSeek,
            "deepseek-v4-flash",
            &working,
            &profile,
            &long_term,
            &long,
            "коротко",
        );
        assert_eq!(body["stream"], json!(false));
        assert_eq!(body["temperature"], json!(0.2));
        assert_eq!(body["max_tokens"], json!(500));
        assert_eq!(body["thinking"]["type"], json!("disabled"), "рассуждение памяти не нужно");
        assert_eq!(body["messages"][0]["content"], json!(MEMORY_PROMPT));
        // Правило маршрутизации живёт прямо в промпте: это и есть «явно
        // выбираем, что куда», а не надежда на здравый смысл модели.
        for rule in ["profile — факты о самом человеке", "kind=decision", "kind=knowledge", "клади в working"] {
            assert!(MEMORY_PROMPT.contains(rule), "в промпте нет правила: {rule}");
        }
        // Рабочую память просим дельтой — на этом держится слияние.
        assert!(MEMORY_PROMPT.contains("появилось или изменилось"), "{MEMORY_PROMPT}");
        assert!(MEMORY_PROMPT.contains("пустой строкой"), "{MEMORY_PROMPT}");

        let user = body["messages"][1]["content"].as_str().expect("вторая реплика — строка");
        assert!(user.starts_with("Рабочая память:\n{\"бюджет\":\"400 тыс. рублей\"}"));
        // Профиль и долговременная уходят ключами (у долговременной ещё и
        // типами): значения в этом вызове не нужны, а дубли модель должна
        // видеть.
        assert!(user.contains("Уже известно про пользователя:\n- роль"), "{user}");
        assert!(user.contains("Уже в долговременной памяти:\n- decision / язык ответов"), "{user}");
        assert!(!user.contains("студент-робототехник"), "значения заметок не отправляем");
        assert_eq!(user.matches('ъ').count(), MEMORY_SOURCE_LIMIT, "длинная реплика обрезана");
        assert!(user.contains("\n\nАссистент: коротко"));

        // Пустые слои дают пустой объект и «нет», а не пустые строки.
        let first = memory_body(
            Provider::Cerebras,
            "qwen-3.8-27b",
            &[],
            &Profile::default(),
            &[],
            "вопрос",
            "ответ",
        );
        assert_eq!(first["max_completion_tokens"], json!(500));
        assert_eq!(first["reasoning_effort"], json!("none"));
        let user = first["messages"][1]["content"].as_str().unwrap();
        assert!(user.starts_with(
            "Рабочая память:\n{}\n\nУже известно про пользователя:\nнет\n\nУже в долговременной памяти:\nнет"
        ));
        assert!(user.contains("\n\nЧеловек: вопрос"));
    }

    #[test]
    fn the_memory_reply_is_split_into_three_baskets() {
        let (working, profile, long_term) = parse_memory(
            r#"{"working":{"бюджет":"400 тысяч","срок":"3 месяца"},
                "profile":[{"key":"роль","value":"студент-робототехник"}],
                "long_term":[{"kind":"decision","key":"язык ответов","value":"русский"},
                             {"kind":"knowledge","key":"порог датчика","value":"17 см"}]}"#,
        )
        .expect("чистый JSON");
        assert_eq!(working.len(), 2);
        assert_eq!(working[0], fact("бюджет", "400 тысяч"));
        // Факты о самом человеке идут в профиль, а не в долговременную: это
        // и есть разница дня 12.
        assert_eq!(profile, vec![fact("роль", "студент-робототехник")]);
        assert_eq!(long_term.len(), 2);
        assert_eq!(long_term[0].kind, Kind::Decision);
        assert_eq!(long_term[0].key, "язык ответов");
        assert_eq!(long_term[1].kind, Kind::Knowledge);

        // Обёртку ```json модель всё равно иногда ставит.
        let fenced = parse_memory("```json\n{\"working\": {\"цель\": \"робот-курьер\"}}\n```")
            .expect("обёртка снята");
        assert_eq!(fenced.0, vec![fact("цель", "робот-курьер")]);
        assert!(fenced.1.is_empty() && fenced.2.is_empty(), "корзины нет — и предложений нет");

        // Неверный kind, пустой ключ и пустое значение выбрасываются молча:
        // один кривой элемент не повод терять остальные. `profile` — теперь
        // неверный kind: заметки о человеке приходят своей корзиной.
        let (_, _, kinds) = parse_memory(
            r#"{"long_term":[{"kind":"телепатия","key":"а","value":"б"},
                             {"kind":"profile","key":"роль","value":"студент"},
                             {"kind":"decision","key":"  ","value":"б"},
                             {"kind":"decision","key":"язык","value":""},
                             {"kind":"knowledge","key":"порог","value":"17 см"}]}"#,
        )
        .expect("список разбирается");
        assert_eq!(kinds.len(), 1, "{kinds:?}");
        assert_eq!(kinds[0].kind, Kind::Knowledge);

        // Та же мерка у заметок профиля, только без типа.
        let (_, notes, _) = parse_memory(
            r#"{"profile":[{"key":"  ","value":"б"},
                           {"key":"уровень","value":""},
                           {"key":"Роль","value":"студент"},
                           {"key":"роль","value":"инженер"},
                           {"key":"язык","value":"Rust"}]}"#,
        )
        .expect("список разбирается");
        assert_eq!(notes, vec![fact("Роль", "студент"), fact("язык", "Rust")], "{notes:?}");

        // Дубль внутри одного ответа — по паре (тип, ключ) без учёта регистра.
        let (_, _, dupes) = parse_memory(
            r#"{"long_term":[{"kind":"decision","key":"Роль","value":"студент"},
                             {"kind":"decision","key":"роль","value":"инженер"},
                             {"kind":"knowledge","key":"роль","value":"другой тип"}]}"#,
        )
        .unwrap();
        assert_eq!(dupes.len(), 2, "{dupes:?}");

        // Числа и прочие не-строки становятся текстом, а не теряются.
        let (mixed, _, _) =
            parse_memory(r#"{"working":{"бюджет": 400000, "шум": true, " ": "пусто", "":"тоже"}}"#)
                .expect("значения приводятся к строке");
        assert_eq!(mixed.len(), 2, "пустой ключ выбрасывается: {mixed:?}");
        assert_eq!(mixed[0].value, "400000");
        assert_eq!(mixed[1].value, "true");

        // Дубль по ключу рабочей памяти — без учёта регистра.
        let (dupes, _, _) =
            parse_memory(r#"{"working":{"Бюджет":"400 тысяч","бюджет":"500 тысяч"}}"#).unwrap();
        assert_eq!(dupes.len(), 1);

        // Всё, что не объект, — ошибка: слои останутся прежними.
        assert!(parse_memory("конечно, вот факты: бюджет 400 тысяч").is_err());
        assert!(parse_memory("[1, 2, 3]").is_err());
        assert!(parse_memory("").is_err());
        assert!(parse_memory(r#"{"working":"бюджет 400 тысяч"}"#).is_err());

        // Пустой ответ разбирается: это «без изменений», а не ошибка. Корзины,
        // которой нет вовсе, тоже достаточно.
        assert_eq!(
            parse_memory(r#"{"working":{},"profile":[],"long_term":[]}"#).unwrap(),
            (vec![], vec![], vec![])
        );
        assert_eq!(parse_memory("{}").unwrap(), (vec![], vec![], vec![]));

        // Пустое значение в working доживает до слияния — там оно забывает факт.
        let (erase, _, _) = parse_memory(r#"{"working":{"бюджет":"  "}}"#).unwrap();
        assert_eq!(erase, vec![fact("бюджет", "")]);
    }

    /// Профиль сам себя не пишет — ровно как долговременная память: заметку
    /// модель предлагает, кнопку нажимает человек.
    #[test]
    fn profile_notes_wait_for_a_button_and_duplicates_are_dropped() {
        let mut profile = Profile::new("Первокурсник".to_string());
        profile.notes.push(Note::new("роль".to_string(), "студент".to_string()));

        let added = add_profile_pending(
            &mut profile,
            vec![
                // Уже записано — в очередь не попадёт даже в другом регистре.
                fact("Роль", "инженер"),
                fact("язык", "Rust"),
            ],
        );
        assert_eq!(added, 1);
        assert_eq!(profile.notes.len(), 1, "заметки служебный вызов не трогает");
        assert_eq!(profile.pending.len(), 1);
        assert_eq!(profile.pending[0].key, "язык");
        assert!(!profile.pending[0].id.is_empty(), "у предложения свой id");

        // Второй ход предлагает то же самое — очередь не растёт.
        assert_eq!(add_profile_pending(&mut profile, vec![fact("Язык", "Rust")]), 0);
        assert_eq!(profile.pending.len(), 1);

        // «Запомнить» переносит предложение в заметки, «нет» — выбрасывает.
        let id = profile.pending[0].id.clone();
        let note = profile.take_pending(&id).expect("предложение нашлось");
        assert_eq!(note.key, "язык");
        assert!(profile.take_pending(&id).is_none(), "второй раз взять нечего");
        profile.notes.push(note);
        assert!(profile.knows("ЯЗЫК"), "ключ известен без учёта регистра");
    }

    /// Долговременная память сама себя не пишет: предложения ложатся в очередь,
    /// а дубли по (тип, ключ) до неё не доходят.
    #[test]
    fn suggestions_wait_in_the_queue_and_duplicates_are_dropped() {
        let mut long_term = LongTerm::default();
        long_term.entries.push(entry(Kind::Decision, "роль", "студент-робототехник"));

        let suggest = |kind: Kind, key: &str, value: &str| Suggestion {
            kind,
            key: key.to_string(),
            value: value.to_string(),
        };

        let added = add_pending(
            &mut long_term,
            vec![
                // Уже записано — в очередь не попадёт даже в другом регистре.
                suggest(Kind::Decision, "Роль", "инженер"),
                suggest(Kind::Decision, "язык ответов", "русский"),
                suggest(Kind::Knowledge, "порог датчика", "17 см"),
            ],
        );
        assert_eq!(added, 2);
        assert_eq!(long_term.entries.len(), 1, "записи служебный вызов не трогает");
        assert_eq!(long_term.pending.len(), 2);
        assert_eq!(long_term.pending[0].key, "язык ответов");
        assert!(!long_term.pending[0].id.is_empty(), "у предложения свой id");

        // Второй ход предлагает то же самое — очередь не растёт.
        let added = add_pending(
            &mut long_term,
            vec![suggest(Kind::Decision, "Язык ответов", "русский")],
        );
        assert_eq!(added, 0);
        assert_eq!(long_term.pending.len(), 2);

        // «Запомнить» переносит предложение в записи.
        let id = long_term.pending[0].id.clone();
        let confirmed = long_term.take_pending(&id).expect("предложение на месте");
        long_term.entries.push(confirmed);
        assert_eq!(long_term.entries.len(), 2);
        assert_eq!(long_term.pending.len(), 1);
    }

    fn fact(key: &str, value: &str) -> Fact {
        Fact { key: key.to_string(), value: value.to_string() }
    }

    #[test]
    fn facts_are_merged_not_replaced() {
        let mut facts = vec![fact("бюджет", "400 тысяч"), fact("срок", "3 месяца")];

        // Пустой объект — «без изменений»: молчание модели память не стирает.
        merge_facts(&mut facts, Vec::new());
        assert_eq!(facts, vec![fact("бюджет", "400 тысяч"), fact("срок", "3 месяца")]);

        // Известный ключ обновляется на месте, новый уходит в конец, а факт,
        // которого в ответе нет, остаётся нетронутым.
        merge_facts(&mut facts, vec![fact("бюджет", "500 тысяч"), fact("шум", "до 60 дБ")]);
        assert_eq!(
            facts,
            vec![fact("бюджет", "500 тысяч"), fact("срок", "3 месяца"), fact("шум", "до 60 дБ")]
        );

        // Регистр и пробелы по краям ключа дубля не создают.
        merge_facts(&mut facts, vec![fact(" Бюджет ", "600 тысяч")]);
        assert_eq!(facts.len(), 3, "{facts:?}");
        assert_eq!(facts[0], fact("бюджет", "600 тысяч"), "ключ и позиция прежние");

        // Пустое значение забывает факт, остальные сдвигаются без потерь.
        merge_facts(&mut facts, vec![fact("срок", "   ")]);
        assert_eq!(facts, vec![fact("бюджет", "600 тысяч"), fact("шум", "до 60 дБ")]);

        // Удалять то, чего нет, можно: ответ модели от этого не становится
        // ошибкой, и пустой факт в память не попадает.
        merge_facts(&mut facts, vec![fact("лидар", "")]);
        assert_eq!(facts.len(), 2, "{facts:?}");
    }

    #[test]
    fn merging_stops_adding_at_the_limit_but_keeps_what_is_stored() {
        let mut facts: Vec<Fact> = (0..WORKING_LIMIT).map(|i| fact(&format!("к{i}"), "старое")).collect();

        merge_facts(&mut facts, vec![fact("к0", "новое"), fact("лишний", "не влезет")]);
        assert_eq!(facts.len(), WORKING_LIMIT, "старые факты лимитом не выбрасываются");
        assert_eq!(facts[0], fact("к0", "новое"), "обновление проходит и на полной памяти");
        assert!(!facts.iter().any(|f| f.key == "лишний"));

        // Освободилось место — следующий новый факт уже добавится.
        merge_facts(&mut facts, vec![fact("к1", ""), fact("лишний", "теперь влезет")]);
        assert_eq!(facts.len(), WORKING_LIMIT);
        assert_eq!(facts[WORKING_LIMIT - 1], fact("лишний", "теперь влезет"));
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
