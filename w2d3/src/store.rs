//! Хранилище чатов на диске. Один чат — один JSON-файл, плюс `state.json`
//! с идентификатором открытого чата. Про HTTP и про API провайдеров здесь
//! не знают: наружу торчат `Chat` и `Store`.
//!
//! Почему файлы, а не SQLite: чатов десятки, читаются они целиком, а файл
//! можно открыть глазами и починить руками. Запись атомарная (tmp + rename),
//! поэтому Ctrl-C посреди сохранения оставляет либо старую версию, либо новую.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::agent::{Message, Metrics, Provider, Settings};

/// Папка данных относительно запуска (`cargo run` из `w2d3`).
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
        }
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
        let cpt = self.chars_per_token.filter(|v| *v > 0.0).unwrap_or(DEFAULT_CHARS_PER_TOKEN);
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

#[derive(Serialize, Deserialize, Default)]
struct StateFile {
    active_chat: Option<String>,
}

pub struct Store {
    root: PathBuf,
    chats: PathBuf,
}

impl Store {
    /// Создаёт папки и сразу читает всё, что там лежит: битые файлы должны
    /// обнаружиться на старте, а не при первом клике.
    pub fn open(root: impl AsRef<Path>) -> Result<Store, String> {
        let root = root.as_ref().to_path_buf();
        let chats = root.join("chats");
        std::fs::create_dir_all(&chats)
            .map_err(|e| format!("не удалось создать {}: {e}", chats.display()))?;
        let store = Store { root, chats };
        let _ = store.list();
        Ok(store)
    }

    /// Все чаты, свежие сверху. Битый файл — строка в stderr и пропуск:
    /// терять весь список из-за одного испорченного файла незачем.
    pub fn list(&self) -> Vec<Chat> {
        let entries = match std::fs::read_dir(&self.chats) {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!("не читается {}: {e}", self.chats.display());
                return Vec::new();
            }
        };
        let mut chats: Vec<Chat> = entries
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
        chats.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        chats
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

    /// Удаляет чат и, если он был открытым, переводит указатель на первый
    /// из оставшихся.
    pub fn delete(&self, id: &str) -> Result<(), String> {
        let path = self.chat_path(id).ok_or("недопустимый id чата")?;
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("не удалось удалить {}: {e}", path.display())),
        }
        if self.read_state().active_chat.as_deref() == Some(id) {
            let next = self.list().first().map(|c| c.id.clone());
            self.write_state(next)?;
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
        self.write_state(Some(id.to_string()))
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

    fn write_state(&self, active_chat: Option<String>) -> Result<(), String> {
        let text = serde_json::to_string_pretty(&StateFile { active_chat })
            .map_err(|e| format!("не сериализуется: {e}"))?;
        write_atomic(&self.state_path(), &text)
    }

    /// Единственное место, где id превращается в путь. Проверка не
    /// косметическая: id приезжает из URL, и `../../.env` пройти не должен.
    fn chat_path(&self, id: &str) -> Option<PathBuf> {
        let ok = !id.is_empty() && id.len() <= 40 && id.chars().all(|c| c.is_ascii_alphanumeric());
        ok.then(|| self.chats.join(format!("{id}.json")))
    }
}

fn read_chat(path: &Path) -> Result<Chat, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)
        .and_then(|_| std::fs::rename(&tmp, path))
        .map_err(|e| format!("не удалось сохранить {}: {e}", path.display()))
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
        let dir = std::env::temp_dir().join(format!("w2d3-{name}-{}-{}", std::process::id(), new_id()));
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
