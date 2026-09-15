//! Хранилище на диске. Три слоя памяти лежат в трёх разных местах — это
//! главное свойство дня 11:
//!
//! - краткосрочная — `chats/<id>.json`, история разговора вместе с настройками;
//! - рабочая — `working/<chat_id>.json`, память текущей задачи, одна на чат;
//! - долговременная — `long_term.json`, одна на всё приложение.
//!
//! Четвёртым лежит профиль пользователя — `profiles/<id>.json`, по файлу на
//! профиль. Это не память: память помнит разговоры, профиль описывает
//! человека, и потому он и хранится отдельно от них.
//!
//! Плюс `state.json` с идентификаторами открытого чата и выбранного профиля.
//! Про HTTP и про API провайдеров здесь не знают: наружу торчат `Chat`,
//! `Working`, `LongTerm`, `Profile` и `Store`.
//!
//! Почему файлы, а не SQLite: чатов десятки, читаются они целиком, а файл
//! можно открыть глазами и починить руками. Запись атомарная (tmp + rename),
//! поэтому Ctrl-C посреди сохранения оставляет либо старую версию, либо новую.

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::agent::{Message, Metrics, Provider, Settings, Strategy};

/// Папка данных относительно запуска (`cargo run` из `w3d2`).
pub const DATA_DIR: &str = "data";

/// Сколько символов заголовка берём из первого вопроса.
const TITLE_LIMIT: usize = 40;

/// Символов на токен, пока чат не откалиброван ни одним ответом. То же число
/// берёт страница до первого ответа.
const DEFAULT_CHARS_PER_TOKEN: f64 = 3.0;

/// Границы правдоподобной калибровки: меньше символа на токен не бывает ни в
/// одном словаре, больше шести — признак того, что `prompt_tokens` посчитан
/// не от того промпта, который мы отправили.
const MIN_CHARS_PER_TOKEN: f64 = 1.0;
const MAX_CHARS_PER_TOKEN: f64 = 6.0;

/// Потолок заголовка ветки: длиннее в список чатов всё равно не влезет.
const BRANCH_TITLE_LIMIT: usize = 60;

/// Заготовка профиля для первого запуска.
struct DemoProfile {
    name: &'static str,
    address: &'static str,
    style: &'static str,
    format: &'static str,
    constraints: &'static str,
    context: &'static str,
    persona: &'static str,
    steps: &'static str,
}

/// Два профиля одного человека и одной темы: разница только в том, кто
/// спрашивает и как ему надо отвечать. На них и показывается день —
/// один вопрос, два разных ответа.
const DEMO_PROFILES: &[DemoProfile] = &[
    DemoProfile {
        name: "Первокурсник",
        address: "на ты, по имени Антон",
        style: "подробно, с аналогиями из жизни, спокойно",
        format: "примеры кода на Python, формулы только простые, markdown",
        constraints: "без матанализа и матричной записи; не предлагать платные инструменты",
        context: "первый курс, робототехника, готовлюсь к лабораторным",
        persona: "robotics",
        steps: "Идея на пальцах\nПростой пример\nМаленькое упражнение на 10 минут\nОдин вопрос на понимание",
    },
    DemoProfile {
        name: "Инженер",
        address: "на вы, без имени",
        style: "кратко, только суть, без вводных",
        format: "код на C++, формулы в LaTeX, без markdown-заголовков",
        constraints: "без упражнений и вопросов на понимание; не пересказывать основы",
        context: "инженер-разработчик встраиваемых систем, проектирую промышленного робота",
        persona: "robotics",
        steps: "Сразу решение\nРиски и ограничения\nСледующий шаг",
    },
];

/// Факт рабочей памяти: короткий ключ и значение. Список, а не словарь, —
/// порядок стабильный, и в таком виде факты показываются в интерфейсе. Ключи
/// сравниваются без учёта регистра и пробелов по краям: модель присылает
/// дельту, и «Бюджет» в ней должен обновить прежний «бюджет», а не лечь
/// рядом вторым фактом.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    pub key: String,
    pub value: String,
}

/// Ключи памяти сравниваются без учёта регистра и пробелов по краям: «Бюджет»
/// и «бюджет » — одно и то же.
pub fn same_key(left: &str, right: &str) -> bool {
    left.trim().to_lowercase() == right.trim().to_lowercase()
}

/// Тип долговременной записи. В дне 11 типов было три, третьим шёл `profile` —
/// факты о самом человеке. В дне 12 они переехали в профиль (`Profile`), и
/// долговременной памяти остались два повода помнить что-то дольше задачи.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Решение, которое человек назвал общим для всех разговоров.
    Decision,
    /// Проверенное знание, которое просили помнить всегда.
    Knowledge,
}

impl Kind {
    pub const ALL: [Kind; 2] = [Kind::Decision, Kind::Knowledge];

    pub fn from_id(id: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.id() == id)
    }

    pub fn id(&self) -> &'static str {
        match self {
            Kind::Decision => "decision",
            Kind::Knowledge => "knowledge",
        }
    }

    /// Как тип называется в интерфейсе — в единственном числе.
    pub fn label(&self) -> &'static str {
        match self {
            Kind::Decision => "решение",
            Kind::Knowledge => "знание",
        }
    }

    /// Заголовок группы в системном сообщении долговременной памяти.
    pub fn group(&self) -> &'static str {
        match self {
            Kind::Decision => "Решения",
            Kind::Knowledge => "Знания",
        }
    }
}

/// Запись долговременной памяти. У неё есть свой id: она живёт дольше чата,
/// из которого пришла, и по индексу в массиве её адресовать нельзя — список
/// правится руками с двух сторон.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub kind: Kind,
    pub key: String,
    pub value: String,
    pub created_at: String,
}

impl Entry {
    pub fn new(kind: Kind, key: String, value: String) -> Entry {
        Entry { id: new_id(), kind, key, value, created_at: utc_now() }
    }
}

/// Рабочая память одной задачи. Один чат — одна задача, поэтому файл
/// адресуется id чата. Счётчики служебных вызовов памяти лежат здесь же:
/// это расход самой памяти, и мешать его с токенами разговора нечестно.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Working {
    #[serde(default)]
    pub facts: Vec<Fact>,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub cost: f64,
}

impl Working {
    /// Что переезжает в ветку: сами факты. Счётчики — расход родителя, и
    /// приписывать его ветке значило бы посчитать одни токены дважды.
    pub fn branched(&self) -> Working {
        Working { facts: self.facts.clone(), ..Working::default() }
    }
}

/// Долговременная память — одна на приложение. `entries` записаны, `pending`
/// только предложены моделью и ждут кнопки человека: сама себе долговременная
/// память ничего не пишет, иначе «явно выбираем, что куда» превращается в
/// «модель решила».
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LongTerm {
    #[serde(default, deserialize_with = "lenient_entries")]
    pub entries: Vec<Entry>,
    #[serde(default, deserialize_with = "lenient_entries")]
    pub pending: Vec<Entry>,
}

/// Записи читаются по одной: элемент с неизвестным `kind` отбрасывается молча.
/// В дне 11 был тип `profile`, и файл, оставшийся от него, не должен ронять
/// весь слой — терять из-за одной строки решения и знания незачем.
fn lenient_entries<'de, D>(deserializer: D) -> Result<Vec<Entry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
    Ok(raw.into_iter().filter_map(|item| serde_json::from_value(item).ok()).collect())
}

impl LongTerm {
    /// Известна ли уже такая пара (тип, ключ) — среди записанного или среди
    /// ожидающего. По ней и отсеиваются дубли в предложениях модели.
    pub fn knows(&self, kind: Kind, key: &str) -> bool {
        self.entries
            .iter()
            .chain(self.pending.iter())
            .any(|e| e.kind == kind && same_key(&e.key, key))
    }

    /// Достать предложение из очереди: «запомнить» переносит его в записи,
    /// «нет» просто выбрасывает.
    pub fn take_pending(&mut self, id: &str) -> Option<Entry> {
        let at = self.pending.iter().position(|e| e.id == id)?;
        Some(self.pending.remove(at))
    }

    /// Перевод факта рабочей памяти в долговременную запись. Дубль по паре
    /// (тип, ключ) — отказ, и факт остаётся в рабочей: исчезнуть из одного слоя,
    /// не появившись в другом, он не должен. Проверка та же, что у ручного
    /// добавления, и делается до того, как что-то сдвинулось.
    pub fn promote(
        &mut self,
        kind: Kind,
        working: &mut Working,
        index: usize,
    ) -> Result<(), String> {
        let fact = working.facts.get(index).ok_or("такого факта нет")?;
        if self.knows(kind, &fact.key) {
            return Err("такая запись уже есть".to_string());
        }
        let fact = working.facts.remove(index);
        self.entries.push(Entry::new(kind, fact.key, fact.value));
        Ok(())
    }
}

/// Заметка профиля: то же, что запись долговременной памяти, но без типа —
/// в профиле всё и так про одного человека. Свой id нужен по той же причине:
/// список правится с двух сторон, и адресовать заметку индексом нельзя.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub id: String,
    pub key: String,
    pub value: String,
    pub created_at: String,
}

impl Note {
    pub fn new(key: String, value: String) -> Note {
        Note { id: new_id(), key, value, created_at: utc_now() }
    }
}

/// Профиль пользователя — нюансы конкретного человека, а не память о
/// разговорах: как к нему обращаться, каким стилем и в каком формате
/// отвечать, чего не делать и в каком порядке разворачивать ответ. Профилей
/// у человека несколько, по одному на род задач: «Первокурсник» и «Инженер»
/// спрашивают одно и то же, а ответы им нужны разные.
///
/// Все текстовые поля могут быть пустыми — пустой профиль просто не даёт
/// блока в запросе. `notes` пополняются с подтверждения человека: служебный
/// вызов памяти кладёт предложения в `pending`, кнопку нажимает человек.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    /// Название профиля в интерфейсе: «Первокурсник», «Инженер».
    pub name: String,
    /// Как обращаться к человеку.
    pub address: String,
    pub style: String,
    pub format: String,
    /// Чего не делать.
    pub constraints: String,
    /// Кто человек и зачем спрашивает.
    pub context: String,
    /// id личности из `PERSONAS`, с которой открывается новый чат. Пустая
    /// строка — личность по умолчанию, как раньше.
    #[serde(default)]
    pub persona: String,
    /// Порядок шагов ответа, по шагу на строку. Мультиагентов у нас нет,
    /// поэтому «пайплайн задачи» — это порядок разделов внутри одного ответа.
    #[serde(default)]
    pub steps: String,
    /// Заметки о человеке, накопленные из разговоров.
    #[serde(default)]
    pub notes: Vec<Note>,
    /// Предложения служебного вызова, ждущие кнопки человека.
    #[serde(default)]
    pub pending: Vec<Note>,
    #[serde(default)]
    pub created_at: String,
}

impl Profile {
    pub fn new(name: String) -> Profile {
        Profile { id: new_id(), name, created_at: utc_now(), ..Profile::default() }
    }

    /// Известен ли уже такой ключ — среди заметок или среди ожидающих.
    /// По нему и отсеиваются дубли в предложениях модели.
    pub fn knows(&self, key: &str) -> bool {
        self.notes.iter().chain(self.pending.iter()).any(|n| same_key(&n.key, key))
    }

    pub fn take_pending(&mut self, id: &str) -> Option<Note> {
        let at = self.pending.iter().position(|n| n.id == id)?;
        Some(self.pending.remove(at))
    }

    /// Строки `steps` без пустых и без пробелов по краям — по строке на шаг.
    pub fn step_lines(&self) -> Vec<&str> {
        self.steps.lines().map(|line| line.trim()).filter(|line| !line.is_empty()).collect()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Chat {
    pub id: String,
    /// Пустой до первого вопроса — список показывает такой чат курсивом.
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    /// Настройки принадлежат чату: у каждого свой провайдер, модель и промпт.
    pub settings: Settings,
    /// Системный промпт сюда не пишется — он подставляется на каждый запрос,
    /// поэтому его правка на ходу не рвёт контекст.
    pub messages: Vec<Message>,
    /// Сколько символов приходится на токен в этом чате: `sent_chars`
    /// последнего запроса делить на его `prompt_tokens`. Нужна для оценки
    /// длины сообщения до отправки — токенизатора у нас нет. `None` — ни
    /// одного ответа ещё не было, клиент берёт умолчание.
    #[serde(default)]
    pub chars_per_token: Option<f64>,
    /// Пересказ начала разговора. Полная история из `messages` никуда не
    /// девается — summary меняет только то, что уходит провайдеру.
    #[serde(default)]
    pub summary: Option<String>,
    /// Сколько первых элементов `messages` покрыто пересказом. Считаем по
    /// индексам массива, а не по репликам, ушедшим в API: в истории бывают
    /// записи с ролью `error`, они провайдеру не отправляются (`goes_to_api`),
    /// но место в нумерации занимают — по индексам однозначно.
    #[serde(default)]
    pub summary_covers: usize,
    /// Токены и стоимость самих суммаризаций — отдельно от токенов разговора,
    /// по тому же принципу, что и у темы чата: это не ответ на вопрос
    /// человека, и мешать их в общих счётчиках было бы враньём.
    #[serde(default)]
    pub summary_prompt_tokens: u64,
    #[serde(default)]
    pub summary_completion_tokens: u64,
    #[serde(default)]
    pub summary_cost: f64,
    /// Чат, от которого эта ветка отпочковалась, и индекс последнего
    /// скопированного сообщения в нём. У обычного чата — `None`. Родителя
    /// могли удалить: тогда ветка остаётся с указателем в пустоту и
    /// показывается корневой, а не пропадает.
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub branch_from: Option<usize>,
}

impl Chat {
    pub fn new(settings: Settings) -> Chat {
        let now = utc_now();
        Chat {
            id: new_id(),
            title: String::new(),
            created_at: now.clone(),
            updated_at: now,
            settings,
            messages: Vec::new(),
            chars_per_token: None,
            summary: None,
            summary_covers: 0,
            summary_prompt_tokens: 0,
            summary_completion_tokens: 0,
            summary_cost: 0.0,
            parent: None,
            branch_from: None,
        }
    }

    /// Ветка от сообщения `at`: копия чата по это сообщение включительно.
    /// Дальше две ветки живут независимо — это и есть способ сравнить два
    /// продолжения одного и того же места разговора.
    ///
    /// Что переезжает: настройки, калибровка и история `messages[..=at]`
    /// вместе с метриками ответов. Пересказ — только если он не рассказывает
    /// о репликах, которых в ветке нет. Рабочая память копируется отдельно
    /// (`Store::branch_working`): она лежит в своём файле. Накопительные
    /// счётчики служебных вызовов обнуляются: это расход родителя, и
    /// приписывать его ветке значило бы посчитать одни токены дважды.
    pub fn branch(&self, at: usize) -> Result<Chat, String> {
        if at >= self.messages.len() {
            return Err(format!("в чате нет сообщения #{}", at + 1));
        }
        let mut branch = Chat::new(self.settings.clone());
        branch.title = branch_title(&self.title, at);
        branch.messages = self.messages[..=at].to_vec();
        branch.chars_per_token = self.chars_per_token;
        if self.summary_covers <= at + 1 {
            branch.summary = self.summary.clone();
            branch.summary_covers = self.summary_covers;
        }
        branch.parent = Some(self.id.clone());
        branch.branch_from = Some(at);
        Ok(branch)
    }

    /// Начало скользящего окна: индекс первого элемента `messages`, который
    /// ещё уходит провайдеру. Считаем по индексам массива, а не по репликам
    /// для API, — по той же причине, что и покрытие пересказа: записи `error`
    /// место в нумерации занимают, и разделитель в ленте встаёт ровно сюда.
    pub fn window_start(&self, keep_last: usize) -> usize {
        self.messages.len().saturating_sub(keep_last)
    }

    /// Символов на токен по калибровке этого чата; до первого ответа —
    /// умолчание. Одно место, чтобы оценки на сервере и на странице считались
    /// от одного числа.
    pub fn chars_per_token_or_default(&self) -> f64 {
        self.chars_per_token.filter(|v| *v > 0.0).unwrap_or(DEFAULT_CHARS_PER_TOKEN)
    }

    /// Пора ли сворачивать историю. Считается после того, как новый вопрос уже
    /// лёг в `messages`, и до основного запроса: свернуть надо ровно то, что
    /// иначе уехало бы провайдеру целиком. Уменьшили `keep_last` или
    /// `summarize_every` — задним числом ничего не пересчитывается, порог
    /// просто сработает на следующем вопросе.
    pub fn needs_summary(&self, settings: &Settings) -> bool {
        if settings.strategy != Strategy::Summary {
            return false;
        }
        let uncovered = self.messages.len().saturating_sub(self.summary_covers);
        uncovered.saturating_sub(settings.keep_last) >= settings.summarize_every
    }

    /// Что уходит суммаризатору: всё непокрытое, кроме хвоста из последних
    /// `keep_last` сообщений. Пустой срез — сворачивать нечего.
    pub fn summary_slice(&self, keep_last: usize) -> &[Message] {
        let end = self.messages.len().saturating_sub(keep_last);
        let start = self.summary_covers.min(end);
        &self.messages[start..end]
    }

    /// Вопрос пользователя. Первый заодно даёт чату заголовок.
    pub fn push_user(&mut self, text: &str) {
        if self.title.is_empty() {
            self.title = make_title(text);
        }
        self.messages.push(Message::new("user", text.to_string()));
        self.updated_at = utc_now();
    }

    pub fn push_assistant(&mut self, text: String, metrics: Option<Metrics>) {
        self.messages.push(Message { metrics, ..Message::new("assistant", text) });
        self.updated_at = utc_now();
    }

    /// Отклонённый запрос. Само сообщение уже откачено (`pop_user`), в
    /// историю ложится только факт отказа с текстом ошибки как есть и с
    /// размером отклонённого сообщения: в символах и в токенах по калибровке
    /// этого чата. Провайдеру такая запись не уходит.
    pub fn push_error(&mut self, error: &str, attempted_chars: usize) {
        let cpt = self.chars_per_token_or_default();
        self.messages.push(Message {
            attempted_tokens: Some((attempted_chars as f64 / cpt).ceil() as u32),
            attempted_chars: Some(attempted_chars as u32),
            ..Message::new("error", error.to_string())
        });
        self.updated_at = utc_now();
    }

    /// Пересчёт «символов на токен» по последнему удавшемуся запросу.
    /// Нулевой `prompt_tokens` (провайдер не прислал usage) оставляет
    /// прежнюю оценку: делить на ноль незачем, а врать нечем.
    ///
    /// Под сжатием роутера калибровки нет вовсе: OpenRouter режет середину
    /// истории, `prompt_tokens` считается от урезанного промпта и стоит на
    /// месте, пока `sent_chars` растёт, — коэффициент раздувался бы с каждым
    /// ходом. Результат вне 1..6 символов на токен по той же причине не
    /// берётся: это не плотность языка, а искажение.
    pub fn calibrate(&mut self, sent_chars: usize, prompt_tokens: u64) {
        if self.settings.router_compression
            && self.settings.provider == Provider::OpenRouter.id()
        {
            return;
        }
        if prompt_tokens == 0 {
            return;
        }
        let cpt = sent_chars as f64 / prompt_tokens as f64;
        if (MIN_CHARS_PER_TOKEN..=MAX_CHARS_PER_TOKEN).contains(&cpt) {
            self.chars_per_token = Some(cpt);
        }
    }

    /// Откат вопроса, оставшегося без ответа: иначе следующий запрос ушёл бы
    /// с двумя user-репликами подряд.
    pub fn pop_user(&mut self, text: &str) {
        if self.messages.last().is_some_and(|m| m.role == "user" && m.content == text) {
            self.messages.pop();
        }
    }
}

/// Указатели приложения: открытый чат и выбранный профиль. Профиль общий на
/// все чаты — он про человека, а не про разговор, и переключение действует на
/// следующий запрос в любом чате.
#[derive(Serialize, Deserialize, Default)]
struct StateFile {
    #[serde(default)]
    active_chat: Option<String>,
    #[serde(default)]
    active_profile: Option<String>,
}

pub struct Store {
    root: PathBuf,
    chats: PathBuf,
    working: PathBuf,
    profiles: PathBuf,
}

impl Store {
    /// Создаёт папки и сразу читает всё, что там лежит: битые файлы должны
    /// обнаружиться на старте, а не при первом клике.
    pub fn open(root: impl AsRef<Path>) -> Result<Store, String> {
        let root = root.as_ref().to_path_buf();
        let chats = root.join("chats");
        let working = root.join("working");
        let profiles = root.join("profiles");
        for dir in [&chats, &working, &profiles] {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("не удалось создать {}: {e}", dir.display()))?;
        }
        let store = Store { root, chats, working, profiles };
        let _ = store.list();
        store.seed_profiles()?;
        Ok(store)
    }

    /// Пустая папка профилей — приложение сеет два демонстрационных: без них
    /// первый запуск показывал бы пустой селект, а весь смысл дня в том, что
    /// один и тот же вопрос под разными профилями получает разные ответы.
    /// Профилей уже хоть один — ничего не трогаем.
    fn seed_profiles(&self) -> Result<(), String> {
        if !self.profiles().is_empty() {
            return Ok(());
        }
        let mut first: Option<String> = None;
        for demo in DEMO_PROFILES {
            let profile = Profile {
                name: demo.name.to_string(),
                address: demo.address.to_string(),
                style: demo.style.to_string(),
                format: demo.format.to_string(),
                constraints: demo.constraints.to_string(),
                context: demo.context.to_string(),
                persona: demo.persona.to_string(),
                steps: demo.steps.to_string(),
                ..Profile::new(String::new())
            };
            self.save_profile(&profile)?;
            first.get_or_insert(profile.id);
        }
        if let Some(id) = first {
            self.set_active_profile(&id)?;
        }
        Ok(())
    }

    /// Все чаты: корневые — свежие сверху, ветки — сразу под своим родителем.
    /// Битый файл — строка в stderr и пропуск: терять весь список из-за одного
    /// испорченного файла незачем.
    pub fn list(&self) -> Vec<Chat> {
        let entries = match std::fs::read_dir(&self.chats) {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!("не читается {}: {e}", self.chats.display());
                return Vec::new();
            }
        };
        let chats: Vec<Chat> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
            .filter_map(|path| match read_chat(&path) {
                Ok(chat) => Some(chat),
                Err(reason) => {
                    eprintln!("пропускаю {}: {reason}", path.display());
                    None
                }
            })
            .collect();
        order_with_branches(chats)
    }

    pub fn load(&self, id: &str) -> Option<Chat> {
        let path = self.chat_path(id)?;
        match read_chat(&path) {
            Ok(chat) => Some(chat),
            Err(reason) => {
                if path.exists() {
                    eprintln!("не читается {}: {reason}", path.display());
                }
                None
            }
        }
    }

    pub fn save(&self, chat: &Chat) -> Result<(), String> {
        let path = self.chat_path(&chat.id).ok_or("недопустимый id чата")?;
        let text = serde_json::to_string_pretty(chat).map_err(|e| format!("не сериализуется: {e}"))?;
        write_atomic(&path, &text)
    }

    /// Удаляет чат вместе с его рабочей памятью и, если он был открытым,
    /// переводит указатель на первый из оставшихся. Долговременную память
    /// удаление чата не трогает: она на то и долговременная.
    pub fn delete(&self, id: &str) -> Result<(), String> {
        let path = self.chat_path(id).ok_or("недопустимый id чата")?;
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("не удалось удалить {}: {e}", path.display())),
        }
        if let Some(path) = self.working_path(id) {
            if let Err(e) = std::fs::remove_file(&path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("не удалось удалить {}: {e}", path.display());
                }
            }
        }
        if self.read_state().active_chat.as_deref() == Some(id) {
            let next = self.list().first().map(|c| c.id.clone());
            let state = StateFile { active_chat: next, ..self.read_state() };
            self.write_state(&state)?;
        }
        Ok(())
    }

    /// Открытый чат. Указатель мог протухнуть (файл удалили руками) — тогда
    /// первый из списка.
    pub fn active(&self) -> Option<String> {
        if let Some(id) = self.read_state().active_chat {
            if self.chat_path(&id).is_some_and(|p| p.exists()) {
                return Some(id);
            }
        }
        self.list().first().map(|c| c.id.clone())
    }

    pub fn set_active(&self, id: &str) -> Result<(), String> {
        let state = StateFile { active_chat: Some(id.to_string()), ..self.read_state() };
        self.write_state(&state)
    }

    fn state_path(&self) -> PathBuf {
        self.root.join("state.json")
    }

    fn read_state(&self) -> StateFile {
        std::fs::read_to_string(self.state_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn write_state(&self, state: &StateFile) -> Result<(), String> {
        let text =
            serde_json::to_string_pretty(state).map_err(|e| format!("не сериализуется: {e}"))?;
        write_atomic(&self.state_path(), &text)
    }

    /// Профили, старые сверху: порядок в селекте не должен прыгать от запуска
    /// к запуску. Битый файл — строка в stderr и пропуск, как у чатов.
    pub fn profiles(&self) -> Vec<Profile> {
        let entries = match std::fs::read_dir(&self.profiles) {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!("не читается {}: {e}", self.profiles.display());
                return Vec::new();
            }
        };
        let mut profiles: Vec<Profile> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
            .filter_map(|path| match read_json::<Profile>(&path) {
                Ok(profile) => Some(profile),
                Err(reason) => {
                    eprintln!("пропускаю {}: {reason}", path.display());
                    None
                }
            })
            .collect();
        profiles.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
        profiles
    }

    pub fn profile(&self, id: &str) -> Option<Profile> {
        let path = self.profile_path(id)?;
        match read_json::<Profile>(&path) {
            Ok(profile) => Some(profile),
            Err(reason) => {
                if path.exists() {
                    eprintln!("не читается {}: {reason}", path.display());
                }
                None
            }
        }
    }

    pub fn save_profile(&self, profile: &Profile) -> Result<(), String> {
        let path = self.profile_path(&profile.id).ok_or("недопустимый id профиля")?;
        let text = serde_json::to_string_pretty(profile)
            .map_err(|e| format!("не сериализуется: {e}"))?;
        write_atomic(&path, &text)
    }

    /// Удаление профиля. Последний не удаляется: без профиля приложению нечего
    /// подставлять в запрос и нечего показывать в селекте, а пустой профиль
    /// человек всегда может сделать сам.
    pub fn delete_profile(&self, id: &str) -> Result<(), String> {
        let path = self.profile_path(id).ok_or("недопустимый id профиля")?;
        if !path.exists() {
            return Err("такого профиля нет".to_string());
        }
        if self.profiles().len() <= 1 {
            return Err("это последний профиль, его не удалить".to_string());
        }
        std::fs::remove_file(&path)
            .map_err(|e| format!("не удалось удалить {}: {e}", path.display()))?;
        if self.read_state().active_profile.as_deref() == Some(id) {
            let next = self.profiles().first().map(|p| p.id.clone());
            let state = StateFile { active_profile: next, ..self.read_state() };
            self.write_state(&state)?;
        }
        Ok(())
    }

    /// Выбранный профиль. Указатель мог протухнуть (файл удалили руками) —
    /// тогда первый из списка.
    pub fn active_profile(&self) -> Option<String> {
        if let Some(id) = self.read_state().active_profile {
            if self.profile_path(&id).is_some_and(|p| p.exists()) {
                return Some(id);
            }
        }
        self.profiles().first().map(|p| p.id.clone())
    }

    /// Профиль, который идёт в запрос. Профилей нет вовсе (папку вычистили
    /// руками) — пустой: блока в запросе тогда просто не будет.
    pub fn active_profile_or_empty(&self) -> Profile {
        self.active_profile().and_then(|id| self.profile(&id)).unwrap_or_default()
    }

    pub fn set_active_profile(&self, id: &str) -> Result<(), String> {
        let state = StateFile { active_profile: Some(id.to_string()), ..self.read_state() };
        self.write_state(&state)
    }

    /// Рабочая память чата. Файла нет — задача ещё не начиналась, это пустая
    /// память, а не ошибка. Битый файл тоже даёт пустую: терять из-за него
    /// доступ к чату незачем, строка в stderr об этом скажет.
    pub fn working(&self, chat: &str) -> Working {
        let Some(path) = self.working_path(chat) else { return Working::default() };
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                eprintln!("не читается {}: {e}", path.display());
                Working::default()
            }),
            Err(_) => Working::default(),
        }
    }

    pub fn save_working(&self, chat: &str, working: &Working) -> Result<(), String> {
        let path = self.working_path(chat).ok_or("недопустимый id чата")?;
        let text = serde_json::to_string_pretty(working)
            .map_err(|e| format!("не сериализуется: {e}"))?;
        write_atomic(&path, &text)
    }

    /// Ветка получает копию рабочей памяти родителя: продолжение того же
    /// разговора должно помнить ту же задачу.
    pub fn branch_working(&self, from: &str, to: &str) -> Result<(), String> {
        self.save_working(to, &self.working(from).branched())
    }

    /// Долговременная память — один файл на всё приложение.
    pub fn long_term(&self) -> LongTerm {
        let path = self.long_term_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                eprintln!("не читается {}: {e}", path.display());
                LongTerm::default()
            }),
            Err(_) => LongTerm::default(),
        }
    }

    pub fn save_long_term(&self, long_term: &LongTerm) -> Result<(), String> {
        let text = serde_json::to_string_pretty(long_term)
            .map_err(|e| format!("не сериализуется: {e}"))?;
        write_atomic(&self.long_term_path(), &text)
    }

    fn long_term_path(&self) -> PathBuf {
        self.root.join("long_term.json")
    }

    /// Единственные два места, где id превращается в путь. Проверка не
    /// косметическая: id приезжает из URL, и `../../.env` пройти не должен.
    fn chat_path(&self, id: &str) -> Option<PathBuf> {
        safe_id(id).then(|| self.chats.join(format!("{id}.json")))
    }

    fn working_path(&self, id: &str) -> Option<PathBuf> {
        safe_id(id).then(|| self.working.join(format!("{id}.json")))
    }

    fn profile_path(&self, id: &str) -> Option<PathBuf> {
        safe_id(id).then(|| self.profiles.join(format!("{id}.json")))
    }
}

fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 40 && id.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Порядок списка чатов: корневые по `updated_at` (свежие сверху), ветки —
/// сразу под своим родителем по `created_at`, то есть в порядке ветвления.
/// Ветка чата, которого больше нет, считается корневой: указатель в пустоту
/// не повод прятать чат из списка.
pub fn order_with_branches(mut chats: Vec<Chat>) -> Vec<Chat> {
    chats.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    let known: HashSet<String> = chats.iter().map(|c| c.id.clone()).collect();

    let mut branches: HashMap<String, Vec<Chat>> = HashMap::new();
    let mut roots: Vec<Chat> = Vec::new();
    for chat in chats {
        match chat.parent.clone().filter(|p| known.contains(p)) {
            Some(parent) => branches.entry(parent).or_default().push(chat),
            None => roots.push(chat),
        }
    }
    for list in branches.values_mut() {
        list.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    }

    let mut out = Vec::with_capacity(roots.len());
    for root in roots {
        append_with_branches(root, &mut branches, &mut out);
    }
    // Кольцо в `parent` невозможно (ветка всегда моложе родителя), но потерять
    // из-за него чат было бы хуже, чем показать его корневым.
    let mut orphans: Vec<Chat> = branches.into_values().flatten().collect();
    orphans.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    out.extend(orphans);
    out
}

fn append_with_branches(chat: Chat, branches: &mut HashMap<String, Vec<Chat>>, out: &mut Vec<Chat>) {
    let id = chat.id.clone();
    out.push(chat);
    for branch in branches.remove(&id).unwrap_or_default() {
        append_with_branches(branch, branches, out);
    }
}

/// Заголовок ветки: чей это разговор и с какого места он разошёлся.
fn branch_title(parent: &str, at: usize) -> String {
    let parent = if parent.trim().is_empty() { "Новый чат" } else { parent };
    let title = format!("{parent} · ветка от #{}", at + 1);
    title.chars().take(BRANCH_TITLE_LIMIT).collect()
}

fn read_chat(path: &Path) -> Result<Chat, String> {
    read_json(path)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// Счётчик сохранений: даёт каждому tmp-файлу своё имя. Общий `…json.tmp`
/// два одновременных сохранения одного файла писали бы вперемешку, и `rename`
/// переносил бы склеенный мусор. Порядок самих записей сериализует замок в
/// `main.rs`, здесь страховка на случай пути мимо него.
static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.{seq:x}.tmp"));
    let result = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, path));
    if result.is_err() {
        // Переименовать не вышло — недописанный tmp оставлять в папке незачем.
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|e| format!("не удалось сохранить {}: {e}", path.display()))
}

/// Время в миллисекундах плюс четыре случайных hex-символа. Миллисекунд
/// хватило бы и одних, но два чата, созданных в одну миллисекунду, тогда
/// затёрли бы друг друга; случайность берём из `RandomState` — он на то и
/// рандомизирован, отдельный крейт ради четырёх символов не нужен.
fn new_id() -> String {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u8(0);
    format!("{millis:x}{:04x}", hasher.finish() & 0xffff)
}

fn make_title(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= TITLE_LIMIT {
        return text;
    }
    let head: String = text.chars().take(TITLE_LIMIT).collect();
    // Режем по последнему пробелу, если от заголовка остаётся хотя бы
    // половина: иначе одно длинное слово превратилось бы в пару букв.
    let cut = match head.rfind(' ') {
        Some(pos) if head[..pos].chars().count() >= TITLE_LIMIT / 2 => &head[..pos],
        _ => head.as_str(),
    };
    format!("{}…", cut.trim_end())
}

struct Date {
    year: i64,
    month: i64,
    day: i64,
}

/// Дата по числу дней от 1970-01-01 (civil_from_days Говарда Хиннанта).
fn civil_from_days(days: i64) -> Date {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    Date { year, month, day }
}

/// ISO-8601 UTC. Целочисленная арифметика вместо `chrono`: строка нужна
/// только чтобы сортировать чаты и показать дату.
pub fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let d = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        d.year,
        d.month,
        d.day,
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Своя папка на каждый тест: тесты идут в потоках одного процесса.
    fn temp_store(name: &str) -> (PathBuf, Store) {
        let dir = std::env::temp_dir().join(format!("w3d2-{name}-{}-{}", std::process::id(), new_id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).expect("временная папка создаётся");
        (dir, store)
    }

    #[test]
    fn ids_and_stamps_look_right() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b, "два чата подряд не должны получить один id");
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()), "id идёт в имя файла: {a}");

        let stamp = utc_now();
        assert_eq!(stamp.len(), 20, "{stamp}");
        assert!(stamp.ends_with('Z') && stamp.contains('T'), "{stamp}");
        // 2026-09-08T00:00:00Z — 1788825600 секунд от эпохи.
        let d = civil_from_days(1_788_825_600 / 86_400);
        assert_eq!((d.year, d.month, d.day), (2026, 9, 8));
    }

    #[test]
    fn title_comes_from_the_first_question() {
        let mut chat = Chat::new(Settings::default());
        assert!(chat.title.is_empty());
        chat.push_user("Запомни число 17");
        assert_eq!(chat.title, "Запомни число 17");

        // Второй вопрос заголовок не меняет.
        chat.push_assistant("Запомнил".to_string(), None);
        chat.push_user("А теперь другое");
        assert_eq!(chat.title, "Запомни число 17");

        let long = make_title("У меня стучит подвеска на неровностях и руль ведёт вправо");
        assert!(long.ends_with('…'), "{long}");
        assert!(long.chars().count() <= TITLE_LIMIT + 1, "{long}");
        assert!(!long.contains("вправо"));
    }

    #[test]
    fn chats_survive_a_round_trip_through_files() {
        let (dir, store) = temp_store("roundtrip");

        let mut chat = Chat::new(Settings::default());
        chat.push_user("Запомни число 17");
        chat.push_assistant("Запомнил, семнадцать.".to_string(), None);
        store.save(&chat).expect("файл записывается");

        let loaded = store.load(&chat.id).expect("чат читается обратно");
        assert_eq!(loaded.id, chat.id);
        assert_eq!(loaded.title, chat.title);
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[1].content, "Запомнил, семнадцать.");
        assert_eq!(loaded.settings.model, chat.settings.model);

        // Свежий Store видит тот же чат: это и есть «переживает перезапуск».
        let reopened = Store::open(&dir).expect("папка открывается повторно");
        assert_eq!(reopened.list().len(), 1);
        assert_eq!(reopened.list()[0].id, chat.id);

        store.delete(&chat.id).expect("файл удаляется");
        assert!(store.load(&chat.id).is_none());
        assert!(store.list().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Чаты, сохранённые до появления счётчиков, должны читаться дальше:
    /// ни метрик у ответа, ни калибровки в них нет.
    #[test]
    fn old_chats_without_metrics_and_calibration_still_load() {
        let json = r#"{"id":"abc123","title":"Старый чат",
            "created_at":"2026-09-01T10:00:00Z","updated_at":"2026-09-01T10:00:00Z",
            "settings":{"provider":"cerebras","model":"qwen-3.8-27b","temperature":0.7,
                "reasoning":"none","persona":"free","system_prompt":"промпт"},
            "messages":[{"role":"user","content":"вопрос"},
                        {"role":"assistant","content":"ответ"}]}"#;
        let chat: Chat = serde_json::from_str(json).expect("старый файл читается");
        assert_eq!(chat.messages.len(), 2);
        assert!(chat.messages[1].metrics.is_none());
        assert_eq!(chat.chars_per_token, None, "до первого ответа калибровки нет");
        // Полей отклонённого запроса в старом файле тоже нет.
        assert_eq!(chat.messages[0].attempted_tokens, None);
        assert_eq!(chat.messages[0].attempted_chars, None);
    }

    /// Отклонённый запрос переживает запись на диск: иначе после переключения
    /// чата ошибка исчезала бы, а счётчики выглядели бы застрявшими.
    #[test]
    fn a_rejected_request_is_kept_in_the_history() {
        let mut chat = Chat::new(Settings::default());
        chat.push_user("Запомни число 17");
        chat.push_assistant("Запомнил".to_string(), None);
        chat.calibrate(400, 100); // 4 символа на токен
        chat.push_error("API вернул 400: maximum context length is 4095 tokens", 4002);

        let last = chat.messages.last().expect("запись добавлена");
        assert_eq!(last.role, "error");
        assert!(last.content.starts_with("API вернул 400"), "текст ошибки как есть: {}", last.content);
        assert_eq!(last.attempted_chars, Some(4002));
        assert_eq!(last.attempted_tokens, Some(1001), "4002 / 4 с округлением вверх");
        assert_eq!(chat.title, "Запомни число 17", "заголовок берётся из первого вопроса");

        let text = serde_json::to_string(&chat).expect("сериализуется");
        let back: Chat = serde_json::from_str(&text).expect("читается обратно");
        assert_eq!(back.messages.len(), 3);
        assert_eq!(back.messages[2].role, "error");
        assert_eq!(back.messages[2].attempted_tokens, Some(1001));
        assert_eq!(back.messages[2].attempted_chars, Some(4002));

        // Обычные реплики от новых полей не распухают.
        let plain = serde_json::to_string(&chat.messages[0]).expect("сериализуется");
        assert!(!plain.contains("attempted"), "{plain}");

        // Без калибровки оценка идёт по умолчанию: 3 символа на токен.
        let mut fresh = Chat::new(Settings::default());
        fresh.push_error("сбой сети", 10);
        assert_eq!(fresh.messages[0].attempted_tokens, Some(4));
    }

    /// Чат из `n` реплик, чередуя вопрос и ответ: для порогов важна длина
    /// массива, а не содержимое.
    fn chat_with(n: usize, settings: Settings) -> Chat {
        let mut chat = Chat::new(settings);
        for i in 0..n {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            chat.messages.push(Message::new(role, format!("реплика {i}")));
        }
        chat
    }

    fn compressing(keep_last: usize, summarize_every: usize) -> Settings {
        Settings {
            strategy: Strategy::Summary,
            keep_last,
            summarize_every,
            ..Settings::default()
        }
    }

    #[test]
    fn the_summary_threshold_counts_uncovered_messages() {
        let settings = compressing(2, 4);

        // 6 сообщений, покрыто 0, хвост 2 — сворачивать ровно 4, это порог.
        assert!(chat_with(6, settings.clone()).needs_summary(&settings));
        assert!(!chat_with(5, settings.clone()).needs_summary(&settings), "на одно меньше порога");

        // Второй заход считается от уже покрытого: 10 − 4 − 2 = 4.
        let mut covered = chat_with(10, settings.clone());
        covered.summary_covers = 4;
        assert!(covered.needs_summary(&settings));
        covered.summary_covers = 5;
        assert!(!covered.needs_summary(&settings));

        // У остальных стратегий порога суммаризации нет вовсе.
        let off = Settings { strategy: Strategy::Window, ..settings.clone() };
        assert!(!chat_with(100, off.clone()).needs_summary(&off));

        // Покрытие больше длины истории — состояние невозможное, но вычитание
        // usize здесь не должно уходить в панику.
        let mut broken = chat_with(3, settings.clone());
        broken.summary_covers = 10;
        assert!(!broken.needs_summary(&settings));
    }

    #[test]
    fn the_summary_slice_takes_everything_but_the_tail() {
        let mut chat = chat_with(6, compressing(2, 4));
        let slice = chat.summary_slice(2);
        assert_eq!(slice.len(), 4);
        assert_eq!(slice[0].content, "реплика 0");
        assert_eq!(slice[3].content, "реплика 3");

        // Уже покрытое второй раз не сворачивается.
        chat.summary_covers = 2;
        let slice = chat.summary_slice(2);
        assert_eq!(slice.len(), 2);
        assert_eq!(slice[0].content, "реплика 2");

        // Хвост длиннее истории и покрытие больше длины — пустой срез.
        assert!(chat.summary_slice(10).is_empty());
        chat.summary_covers = 10;
        assert!(chat.summary_slice(2).is_empty());
    }

    /// Чат, сохранённый до появления сжатия, должен читаться дальше: полей
    /// пересказа в нём нет, настроек сжатия — тоже.
    #[test]
    fn old_chats_without_summary_fields_still_load() {
        let json = r#"{"id":"abc123","title":"Старый чат",
            "created_at":"2026-09-01T10:00:00Z","updated_at":"2026-09-01T10:00:00Z",
            "settings":{"provider":"cerebras","model":"qwen-3.8-27b","temperature":0.7,
                "reasoning":"none","persona":"free","system_prompt":"промпт"},
            "messages":[{"role":"user","content":"вопрос"}]}"#;
        let chat: Chat = serde_json::from_str(json).expect("старый файл читается");
        assert_eq!(chat.summary, None);
        assert_eq!(chat.summary_covers, 0);
        assert_eq!(chat.summary_prompt_tokens, 0);
        assert_eq!(chat.summary_completion_tokens, 0);
        assert_eq!(chat.summary_cost, 0.0);
        assert_eq!(chat.settings.strategy, Strategy::Full, "старый чат сжатия не просил");
        assert_eq!(chat.settings.keep_last, 6);
        assert_eq!(chat.settings.summarize_every, 10);
        assert!(!chat.needs_summary(&chat.settings));
        assert_eq!(chat.settings.layers, crate::agent::Layers::default(), "слои включены");
        // Полей веток в таком файле тоже нет.
        assert_eq!(chat.parent, None);
        assert_eq!(chat.branch_from, None);
    }

    /// Ветка — снимок разговора до выбранного сообщения. Родитель при этом не
    /// меняется вовсе: две ветки одного места и есть смысл затеи.
    #[test]
    fn a_branch_copies_the_history_up_to_the_chosen_message() {
        let mut parent = Chat::new(compressing(2, 4));
        parent.push_user("Собираем ТЗ");
        parent.push_assistant("Давай уточним".to_string(), None);
        parent.push_user("Бюджет 400 тысяч");
        parent.push_assistant("Записал".to_string(), None);
        parent.chars_per_token = Some(3.5);
        parent.summary = Some("говорили о ТЗ".to_string());
        parent.summary_covers = 2;
        parent.summary_prompt_tokens = 800;
        parent.summary_cost = 0.01;

        let branch = parent.branch(1).expect("сообщение #2 существует");
        assert_eq!(branch.messages.len(), 2);
        assert_eq!(branch.messages[1].content, "Давай уточним");
        assert_eq!(branch.parent.as_deref(), Some(parent.id.as_str()));
        assert_eq!(branch.branch_from, Some(1));
        assert_eq!(branch.title, "Собираем ТЗ · ветка от #2");
        assert_eq!(branch.chars_per_token, Some(3.5));
        assert_eq!(branch.settings.keep_last, 2, "настройки наследуются");
        // Расход родителя на служебные вызовы веткой не наследуется.
        assert_eq!(branch.summary_prompt_tokens, 0);
        assert_eq!(branch.summary_cost, 0.0);
        // Пересказ покрывает ровно скопированное — переезжает.
        assert_eq!(branch.summary.as_deref(), Some("говорили о ТЗ"));
        assert_eq!(branch.summary_covers, 2);

        // Ветка от первого сообщения: пересказ рассказывал бы о том, чего в
        // ветке нет, — он не копируется.
        let early = parent.branch(0).expect("сообщение #1 существует");
        assert_eq!(early.messages.len(), 1);
        assert_eq!(early.summary, None);
        assert_eq!(early.summary_covers, 0);
        assert_eq!(early.title, "Собираем ТЗ · ветка от #1");

        // Родитель не изменился, а сообщения вне диапазона нет.
        assert_eq!(parent.messages.len(), 4);
        assert_eq!(parent.parent, None);
        assert!(parent.branch(4).is_err());
        assert!(Chat::new(Settings::default()).branch(0).is_err(), "в пустом чате ветвиться не от чего");

        let long = branch_title(&"я".repeat(80), 3);
        assert_eq!(long.chars().count(), BRANCH_TITLE_LIMIT);
    }

    #[test]
    fn branches_stand_under_their_parent_in_the_list() {
        let root = |id: &str, updated: &str| Chat {
            id: id.to_string(),
            updated_at: updated.to_string(),
            created_at: updated.to_string(),
            ..Chat::new(Settings::default())
        };
        let branch = |id: &str, parent: &str, created: &str| Chat {
            parent: Some(parent.to_string()),
            branch_from: Some(1),
            ..root(id, created)
        };

        let ordered = order_with_branches(vec![
            branch("b2", "a", "2026-09-10T12:00:00Z"),
            root("a", "2026-09-10T10:00:00Z"),
            root("c", "2026-09-11T10:00:00Z"),
            branch("b1", "a", "2026-09-10T11:00:00Z"),
            // Ветка удалённого чата остаётся в списке как обычный корневой.
            branch("orphan", "нет-такого", "2026-09-09T10:00:00Z"),
        ]);
        let ids: Vec<&str> = ordered.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["c", "a", "b1", "b2", "orphan"]);
    }

    #[test]
    fn the_window_starts_keep_last_messages_from_the_end() {
        let chat = chat_with(6, Settings { strategy: Strategy::Window, ..Settings::default() });
        assert_eq!(chat.window_start(2), 4);
        assert_eq!(chat.window_start(6), 0);
        assert_eq!(chat.window_start(50), 0, "окно длиннее истории её не режет");
    }

    #[test]
    fn calibration_needs_a_non_zero_token_count() {
        let mut chat = Chat::new(Settings::default());
        chat.calibrate(900, 300);
        assert_eq!(chat.chars_per_token, Some(3.0));

        // Провайдер не прислал usage — прежняя оценка остаётся, а не рушится.
        chat.calibrate(4000, 0);
        assert_eq!(chat.chars_per_token, Some(3.0));

        chat.calibrate(1000, 250);
        assert_eq!(chat.chars_per_token, Some(4.0), "калибровка идёт по последнему запросу");
    }

    /// Сжатый роутером промпт плотность текста не показывает, а разбухший
    /// коэффициент занижал бы оценку до отправки.
    #[test]
    fn compressed_and_implausible_calibrations_are_ignored() {
        let mut chat = Chat::new(Settings::default());
        chat.calibrate(900, 300);
        assert_eq!(chat.chars_per_token, Some(3.0));

        // 40 символов на токен — так не токенизирует никто.
        chat.calibrate(40_000, 1_000);
        assert_eq!(chat.chars_per_token, Some(3.0), "неправдоподобное значение не принимается");

        chat.calibrate(2_900, 1_000);
        assert_eq!(chat.chars_per_token, Some(2.9), "правдоподобное — принимается");

        let compressed = Settings {
            provider: "openrouter".to_string(),
            router_compression: true,
            ..Settings::default()
        };
        let mut chat = Chat::new(compressed);
        chat.calibrate(900, 300);
        assert_eq!(chat.chars_per_token, None, "под сжатием коэффициент не трогаем");
    }

    #[test]
    fn broken_files_are_skipped_and_ids_from_urls_are_checked() {
        let (dir, store) = temp_store("broken");
        let chat = Chat::new(Settings::default());
        store.save(&chat).expect("файл записывается");
        std::fs::write(dir.join("chats").join("beef.json"), "{это не json").unwrap();

        let list = store.list();
        assert_eq!(list.len(), 1, "битый файл не должен ронять список");
        assert_eq!(list[0].id, chat.id);

        // Путь из id собирается только для безопасного id.
        assert!(store.load("../../.env").is_none());
        assert!(store.load("").is_none());
        assert!(store.save(&Chat { id: "../boom".to_string(), ..chat.clone() }).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn fact(key: &str, value: &str) -> Fact {
        Fact { key: key.to_string(), value: value.to_string() }
    }

    /// Три слоя — три разных места на диске. Это и есть модель памяти дня 11:
    /// краткосрочная в файле чата, рабочая в своей папке, долговременная одна
    /// на приложение.
    #[test]
    fn the_three_layers_live_in_three_separate_files() {
        let (dir, store) = temp_store("layers");

        let mut chat = Chat::new(Settings::default());
        chat.push_user("Собираем ТЗ");
        chat.push_assistant("Давай уточним".to_string(), None);
        store.save(&chat).unwrap();

        let working = Working {
            facts: vec![fact("бюджет", "400 тысяч")],
            prompt_tokens: 300,
            cost: 0.02,
            ..Working::default()
        };
        store.save_working(&chat.id, &working).unwrap();

        let mut long_term = LongTerm::default();
        long_term.entries.push(Entry::new(
            Kind::Decision,
            "роль".to_string(),
            "студент-робототехник".to_string(),
        ));
        store.save_long_term(&long_term).unwrap();

        assert!(dir.join("chats").join(format!("{}.json", chat.id)).exists());
        assert!(dir.join("working").join(format!("{}.json", chat.id)).exists());
        assert!(dir.join("long_term.json").exists());

        // В файле чата рабочей и долговременной памяти нет вовсе.
        let raw = std::fs::read_to_string(dir.join("chats").join(format!("{}.json", chat.id))).unwrap();
        assert!(!raw.contains("бюджет"), "{raw}");
        assert!(!raw.contains("роль"), "{raw}");

        // Рабочая память читается обратно целиком, вместе со счётчиками.
        let back = store.working(&chat.id);
        assert_eq!(back.facts, working.facts);
        assert_eq!(back.prompt_tokens, 300);
        assert_eq!(back.cost, 0.02);

        // Чата, у которого задачи ещё не было, — пустая память, а не ошибка.
        assert!(store.working("нетакого").facts.is_empty());

        // Удаление чата уносит его рабочую память и не трогает долговременную.
        store.delete(&chat.id).unwrap();
        assert!(!dir.join("working").join(format!("{}.json", chat.id)).exists());
        assert!(store.working(&chat.id).facts.is_empty());
        assert_eq!(store.long_term().entries.len(), 1, "долговременная переживает удаление чата");
        assert_eq!(store.long_term().entries[0].key, "роль");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_branch_gets_a_copy_of_the_working_memory() {
        let (dir, store) = temp_store("branch-working");

        let mut parent = Chat::new(Settings::default());
        parent.push_user("Собираем ТЗ");
        parent.push_assistant("Давай уточним".to_string(), None);
        store.save(&parent).unwrap();
        store
            .save_working(
                &parent.id,
                &Working {
                    facts: vec![fact("бюджет", "400 тысяч"), fact("срок", "3 месяца")],
                    prompt_tokens: 300,
                    completion_tokens: 40,
                    cost: 0.02,
                },
            )
            .unwrap();

        let branch = parent.branch(1).expect("сообщение #2 существует");
        store.save(&branch).unwrap();
        store.branch_working(&parent.id, &branch.id).unwrap();

        let copied = store.working(&branch.id);
        assert_eq!(copied.facts.len(), 2, "факты задачи переезжают в ветку");
        assert_eq!(copied.facts[0], fact("бюджет", "400 тысяч"));
        // Расход родителя на служебные вызовы веткой не наследуется.
        assert_eq!(copied.prompt_tokens, 0);
        assert_eq!(copied.completion_tokens, 0);
        assert_eq!(copied.cost, 0.0);

        // Дальше памяти живут порознь: ветка забыла — у родителя осталось.
        let mut mine = copied;
        mine.facts.clear();
        store.save_working(&branch.id, &mine).unwrap();
        assert_eq!(store.working(&parent.id).facts.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_suggestions_are_told_apart_from_written_entries() {
        let mut long_term = LongTerm::default();
        long_term.entries.push(Entry::new(Kind::Decision, "роль".to_string(), "студент".to_string()));
        long_term.pending.push(Entry::new(
            Kind::Decision,
            "Язык ответов".to_string(),
            "русский".to_string(),
        ));

        assert!(long_term.knows(Kind::Decision, " Роль "), "регистр и пробелы дубля не создают");
        assert!(long_term.knows(Kind::Decision, "язык ответов"), "ожидающее тоже считается");
        assert!(!long_term.knows(Kind::Knowledge, "роль"), "другой тип — другая запись");
        assert!(!long_term.knows(Kind::Decision, "уровень"));

        let id = long_term.pending[0].id.clone();
        let taken = long_term.take_pending(&id).expect("предложение нашлось");
        assert_eq!(taken.key, "Язык ответов");
        assert!(long_term.pending.is_empty());
        assert!(long_term.take_pending(&id).is_none(), "второй раз взять нечего");
        assert_eq!(Kind::from_id("decision"), Some(Kind::Decision));
        assert_eq!(Kind::from_id("нет-такого"), None);
        // Тип `profile` был в дне 11 и остался только в старых файлах:
        // заметки о человеке теперь живут в профиле.
        assert_eq!(Kind::from_id("profile"), None);
    }

    /// Перевод факта в долговременную: дубль отклоняется до того, как факт
    /// уйдёт из рабочей памяти. Иначе он пропал бы из обоих слоёв разом.
    #[test]
    fn promoting_a_duplicate_is_refused_and_keeps_the_working_fact() {
        let mut long_term = LongTerm::default();
        long_term.entries.push(Entry::new(
            Kind::Decision,
            "Роль".to_string(),
            "студент".to_string(),
        ));
        let mut working =
            Working { facts: vec![fact("роль", "инженер"), fact("бюджет", "400 тысяч")], ..Working::default() };

        // Тот же тип и тот же ключ (регистр не в счёт) — отказ той же строкой,
        // что и у ручного добавления.
        let refused = long_term.promote(Kind::Decision, &mut working, 0);
        assert_eq!(refused, Err("такая запись уже есть".to_string()));
        assert_eq!(working.facts.len(), 2, "факт остался в рабочей: {:?}", working.facts);
        assert_eq!(long_term.entries.len(), 1, "в долговременной ничего не прибавилось");

        // Ожидающее предложение считается так же: дважды в очередь не встанет.
        long_term.pending.push(Entry::new(
            Kind::Knowledge,
            "бюджет".to_string(),
            "400 тысяч".to_string(),
        ));
        assert!(long_term.promote(Kind::Knowledge, &mut working, 1).is_err());
        assert_eq!(working.facts.len(), 2);

        // Другой тип — другая запись: перевод проходит, из рабочей факт уходит.
        long_term.promote(Kind::Knowledge, &mut working, 0).expect("дубля нет");
        assert_eq!(working.facts, vec![fact("бюджет", "400 тысяч")]);
        assert_eq!(long_term.entries.len(), 2);
        assert_eq!(long_term.entries[1].kind, Kind::Knowledge);
        assert_eq!(long_term.entries[1].key, "роль");
        assert_eq!(long_term.entries[1].value, "инженер");

        // Факта с таким индексом нет — отказ, и списки не тронуты.
        assert!(long_term.promote(Kind::Decision, &mut working, 9).is_err());
        assert_eq!(working.facts.len(), 1);
    }

    /// Профиль — отдельный файл и отдельный указатель в state.json: его
    /// переживают и удаление чата, и перезапуск.
    #[test]
    fn a_profile_survives_a_round_trip_through_the_files() {
        let (dir, store) = temp_store("profile");

        let mut profile = Profile::new("Инженер".to_string());
        profile.address = "на вы, без имени".to_string();
        profile.steps = "Сразу решение\nРиски".to_string();
        profile.persona = "robotics".to_string();
        profile.notes.push(Note::new("язык".to_string(), "Rust".to_string()));
        profile.pending.push(Note::new("уровень".to_string(), "senior".to_string()));
        store.save_profile(&profile).unwrap();
        store.set_active_profile(&profile.id).unwrap();

        assert!(dir.join("profiles").join(format!("{}.json", profile.id)).exists());
        let back = store.profile(&profile.id).expect("профиль читается обратно");
        assert_eq!(back.name, "Инженер");
        assert_eq!(back.address, "на вы, без имени");
        assert_eq!(back.step_lines(), vec!["Сразу решение", "Риски"]);
        assert_eq!(back.notes.len(), 1);
        assert_eq!(back.pending[0].key, "уровень");

        // Указатель на активный профиль лежит рядом с указателем на чат и
        // друг друга они не затирают.
        let chat = Chat::new(Settings::default());
        store.save(&chat).unwrap();
        store.set_active(&chat.id).unwrap();
        assert_eq!(store.active_profile().as_deref(), Some(profile.id.as_str()));
        assert_eq!(store.active().as_deref(), Some(chat.id.as_str()));
        assert_eq!(store.active_profile_or_empty().name, "Инженер");

        // Удаление чата профиль не трогает: он не про разговор, а про человека.
        store.delete(&chat.id).unwrap();
        assert_eq!(store.active_profile().as_deref(), Some(profile.id.as_str()));

        // Последний профиль не удаляется — иначе подставлять в запрос было бы
        // нечего. Демонстрационных профилей два, поэтому один уходит.
        let ids: Vec<String> = store.profiles().iter().map(|p| p.id.clone()).collect();
        assert_eq!(ids.len(), 3, "два посеянных и свой");
        store.delete_profile(&ids[0]).unwrap();
        store.delete_profile(&ids[1]).unwrap();
        let last = store.profiles();
        assert_eq!(last.len(), 1);
        assert_eq!(
            store.delete_profile(&last[0].id),
            Err("это последний профиль, его не удалить".to_string())
        );
        // Удалили активный — активным становится оставшийся.
        assert_eq!(store.active_profile().as_deref(), Some(last[0].id.as_str()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Пустая папка профилей — приложение сеет два демонстрационных и делает
    /// активным первый. Профили уже есть — посев не повторяется.
    #[test]
    fn an_empty_folder_gets_two_demo_profiles() {
        let (dir, store) = temp_store("seed");
        let profiles = store.profiles();
        assert_eq!(profiles.len(), 2, "{:?}", profiles.iter().map(|p| &p.name).collect::<Vec<_>>());
        let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"Первокурсник") && names.contains(&"Инженер"), "{names:?}");
        assert!(profiles.iter().all(|p| !p.address.is_empty() && !p.steps.is_empty()));
        assert_eq!(
            store.active_profile_or_empty().name,
            "Первокурсник",
            "активным становится первый"
        );

        // Второй запуск на той же папке ничего не добавляет.
        let again = Store::open(&dir).expect("папка открывается второй раз");
        assert_eq!(again.profiles().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Файл долговременной памяти из дня 11 не должен ронять слой: записи с
    /// типом `profile` отбрасываются, решения и знания остаются.
    #[test]
    fn entries_with_an_unknown_kind_are_dropped_on_load() {
        let (dir, store) = temp_store("oldkind");
        let raw = r#"{"entries":[
            {"id":"a1","kind":"profile","key":"роль","value":"студент","created_at":"2026-09-08T00:00:00Z"},
            {"id":"a2","kind":"decision","key":"язык ответов","value":"русский","created_at":"2026-09-08T00:00:00Z"}],
            "pending":[
            {"id":"b1","kind":"profile","key":"стиль","value":"кратко","created_at":"2026-09-08T00:00:00Z"},
            {"id":"b2","kind":"knowledge","key":"порог","value":"17 см","created_at":"2026-09-08T00:00:00Z"}]}"#;
        std::fs::write(dir.join("long_term.json"), raw).unwrap();

        let long_term = store.long_term();
        assert_eq!(long_term.entries.len(), 1, "{:?}", long_term.entries);
        assert_eq!(long_term.entries[0].kind, Kind::Decision);
        assert_eq!(long_term.pending.len(), 1);
        assert_eq!(long_term.pending[0].kind, Kind::Knowledge);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn active_chat_falls_back_after_a_delete() {
        let (dir, store) = temp_store("active");
        assert_eq!(store.active(), None, "пустая папка — открывать нечего");

        let first = Chat::new(Settings::default());
        store.save(&first).unwrap();
        // Второй чат делаем заведомо свежее первого: список сортируется по
        // updated_at, а два Chat::new подряд попадают в одну секунду.
        let mut second = Chat::new(Settings::default());
        second.updated_at = "2099-01-01T00:00:00Z".to_string();
        store.save(&second).unwrap();

        store.set_active(&second.id).unwrap();
        assert_eq!(store.active().as_deref(), Some(second.id.as_str()));

        store.delete(&second.id).unwrap();
        assert_eq!(store.active().as_deref(), Some(first.id.as_str()), "открытым становится оставшийся");

        store.delete(&first.id).unwrap();
        assert_eq!(store.active(), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
