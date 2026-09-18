//! Хранилище на диске. Три слоя памяти лежат в трёх разных местах — это
//! главное свойство дня 11:
//!
//! - краткосрочная — `chats/<id>.json`, история разговора вместе с настройками;
//! - рабочая — `working/<chat_id>.json`, память текущей задачи, одна на чат;
//!   там же лежит и состояние самой задачи (`Task`): этап, шаг, ожидаемое
//!   действие, план с отметками проверки и журнал переходов — чат и есть
//!   задача;
//! - долговременная — `long_term.json`, одна на всё приложение.
//!
//! Четвёртыми лежат инструкции пользователя — `instructions.md`, один текст
//! на приложение, как custom instructions у чат-ассистентов. Это не память:
//! память помнит разговоры, инструкции пишет человек о себе сам.
//!
//! Пятым — инварианты проекта, `invariants.json`, один набор на приложение.
//! Это тоже не память: их пишет только человек, и модель их не пополняет.
//!
//! Плюс `state.json` с идентификатором открытого чата.
//! Про HTTP и про API провайдеров здесь не знают: наружу торчат `Chat`,
//! `Working`, `LongTerm`, `Invariants` и `Store`.
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

/// Папка данных относительно запуска (`cargo run` из `w3d5`).
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

/// Инструкции для первого запуска. Один свободный текст вместо полей профиля:
/// человеку проще написать о себе абзац, чем раскладывать себя по графам.
const DEFAULT_INSTRUCTIONS: &str = "Я изучаю робототехнику и Rust. Обращайся на «ты». Отвечай коротко и по делу, без воды; код — только когда он нужен. Если чего-то не знаешь или не уверен — так и скажи, не выдумывай.";

/// Заготовка инвариантов для первого запуска: учебный робот кружка. Каждый
/// инвариант нарочно такой, что его легко нарушить обычным советом из
/// интернета, — на этом и показывается день.
const DEMO_INVARIANTS: &[(Category, &str, &str)] = &[
    (
        Category::Stack,
        "Прошивка робота — только Rust (embassy). Arduino, C/C++ и MicroPython не предлагать",
        "команда учит Rust, один язык на весь проект",
    ),
    (
        Category::Architecture,
        "Контроллер — ESP32-C3. Raspberry Pi и другие одноплатные компьютеры не предлагать",
        "низкое энергопотребление, всё управление на микроконтроллере",
    ),
    (
        Category::Decision,
        "Питание — только 5 В от USB, без Li-Po и других аккумуляторов",
        "учебный стенд, безопасность",
    ),
    (
        Category::Business,
        "Бюджет на компоненты — не больше 3000 ₽ на весь робот",
        "грант кружка",
    ),
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
/// факты о самом человеке. Теперь о себе человек пишет сам в инструкциях, и
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

/// Этап задачи. Четыре штуки, порядок в массиве — порядок в жизни задачи:
/// по нему считается «2 из 4» в блоке запроса и рисуется полоска в панели.
///
/// Правило этапа (`rule`) лежит здесь, а не в системном промпте, по той же
/// причине, что и таблица переходов: промпт человек правит руками, а история
/// теряется при суммаризации — код не теряется.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    #[default]
    Planning,
    Execution,
    Validation,
    Done,
}

impl Stage {
    pub const ALL: [Stage; 4] = [Stage::Planning, Stage::Execution, Stage::Validation, Stage::Done];

    pub fn from_id(id: &str) -> Option<Stage> {
        Stage::ALL.into_iter().find(|s| s.id() == id)
    }

    pub fn id(&self) -> &'static str {
        match self {
            Stage::Planning => "planning",
            Stage::Execution => "execution",
            Stage::Validation => "validation",
            Stage::Done => "done",
        }
    }

    /// Как этап называется в интерфейсе и в блоке запроса.
    pub fn label(&self) -> &'static str {
        match self {
            Stage::Planning => "планирование",
            Stage::Execution => "выполнение",
            Stage::Validation => "проверка",
            Stage::Done => "готово",
        }
    }

    /// Что агенту можно делать на этом этапе. Уходит в запрос строкой
    /// «Правило этапа: …».
    pub fn rule(&self) -> &'static str {
        match self {
            Stage::Planning => "собирай требования и ограничения, задавай уточняющие вопросы по одному, решений и реализации не предлагай; когда требований хватает — выдай план отдельным блоком ```plan, а утвердит его человек кнопкой",
            Stage::Execution => "выполняй утверждённый план по шагам: принимай решения и предлагай варианты по уже собранным требованиям, не выдумывая новых ограничений",
            Stage::Validation => "пройди по каждому пункту утверждённого плана и про каждый скажи, выполнен ли он и почему; ищи потерянное и противоречия, ничего нового не добавляй",
            Stage::Done => "задача закрыта: отвечай на вопросы по ней и не начинай новую работу",
        }
    }

    /// Что на этапе запрещено — пункт «E» для валидатора ответа. Формулировка
    /// для планирования нарочно называет, что НЕ нарушение: вопросы и план
    /// иначе ловились бы как «готовое решение». План прозой — нарушение
    /// формата: код его не разберёт, и утвердить его будет нечего; тогда
    /// перегенерация попросит оформить план блоком. Вопрос с пронумерованными
    /// вариантами ответа планом не считается — иначе ловился бы каждый
    /// уточняющий вопрос.
    pub fn forbidden(&self) -> &'static str {
        match self {
            Stage::Planning => "нельзя писать реализацию — код, схемы подключения, готовое решение; нельзя предлагать план работ — пронумерованную последовательность шагов, которые предстоит сделать, — вне блока ```plan: план оформляется только блоком ```plan. НЕ нарушение: уточняющие вопросы, в том числе с пронумерованными вариантами ответа («1. вариант А, 2. вариант Б»), и план в блоке ```plan; технические шаги внутри блока ```plan (что настроить, какие пины, задержки, какие библиотеки) — это план, а не реализация",
            Stage::Execution => "нельзя объявлять задачу проверенной или завершённой",
            Stage::Validation => "нельзя добавлять новую функциональность",
            Stage::Done => "нельзя начинать новую работу",
        }
    }

    /// Подпись условия на стрелке, ведущей в этот этап вперёд. Сами условия
    /// проверяет `Task::condition`; здесь только слова для схемы в панели.
    pub fn gate(&self) -> Option<&'static str> {
        match self {
            Stage::Execution => Some("план утверждён"),
            Stage::Done => Some("все пункты ✓"),
            Stage::Planning | Stage::Validation => None,
        }
    }

    /// Разрешённые переходы — единственная таблица на всё приложение.
    /// Назад из выполнения и проверки можно: требования всплывают поздно.
    /// `done` конечный: закрытую задачу переоткрывают новым чатом.
    pub fn allowed(&self) -> &'static [Stage] {
        match self {
            Stage::Planning => &[Stage::Execution],
            Stage::Execution => &[Stage::Validation, Stage::Planning],
            Stage::Validation => &[Stage::Done, Stage::Execution],
            Stage::Done => &[],
        }
    }

    /// Номер этапа с единицы — для строки «2 из 4» в блоке запроса.
    pub fn position(&self) -> usize {
        Stage::ALL.iter().position(|s| s == self).unwrap_or(0) + 1
    }
}

/// Запись журнала переходов: кто, откуда, куда, когда и с какой пометкой.
/// Отклонённая попытка тоже попадает сюда — по журналу должно быть видно, что
/// агент рвался вперёд, а код его не пустил.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    pub from: Stage,
    pub to: Stage,
    pub at: String,
    /// `agent` — предложил служебный вызов памяти, `human` — кнопка в панели.
    pub by: String,
    pub note: String,
}

/// Пометка отклонённой попытки. Одна строка на весь код: по ней журнал и
/// фильтруется глазами.
pub const REJECTED_NOTE: &str = "отклонено: переход запрещён";

/// Пометка попытки, у которой ребро в таблице есть, а условие не выполнено.
/// В журнал за ней через тире идёт, чего именно не хватило.
pub const CONDITION_NOTE: &str = "отклонено: условие не выполнено";

/// Отказ утверждения устаревшей карточки: человек утверждает ровно то, что
/// прочитал, а черновик за это время успел смениться. Сервер отвечает 409.
pub const STALE_PLAN: &str = "план изменился — перечитай карточку";

/// Отметка проверки одного пункта плана: прошёл ли он и почему.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub ok: bool,
    #[serde(default)]
    pub note: String,
}

/// Пункт плана. Текст — ровно то, что человек прочитал в блоке ```plan;
/// отметку проверки ставит служебный вызов памяти на этапе проверки.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanStep {
    pub text: String,
    #[serde(default)]
    pub check: Option<Check>,
}

/// Состояние задачи как конечный автомат: этап, текущий шаг, ожидаемое
/// действие и журнал переходов. Лежит в рабочей памяти чата — чат и есть
/// задача, а значит перезапуск сервера состояние не теряет.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Task {
    #[serde(default)]
    pub stage: Stage,
    /// Что делается прямо сейчас, одной фразой.
    #[serde(default)]
    pub step: String,
    /// Какое действие ожидается дальше и от кого.
    #[serde(default)]
    pub expected: String,
    /// Когда задачу поставили на паузу. `None` — она идёт.
    #[serde(default)]
    pub paused_at: Option<String>,
    /// С какого момента задачу только что продолжили. Флаг на один запрос:
    /// он добавляет в блок строку «не пересказывай заново» и снимается сразу
    /// после этого запроса.
    #[serde(default)]
    pub resumed_from: Option<String>,
    /// План из последнего блока ```plan ответа агента. До утверждения —
    /// черновик, который следующий блок заменяет; после — заморожен.
    #[serde(default)]
    pub plan: Vec<PlanStep>,
    /// Когда человек утвердил план кнопкой. `None` — план черновик.
    #[serde(default)]
    pub plan_approved_at: Option<String>,
    #[serde(default)]
    pub log: Vec<Transition>,
}

impl Task {
    /// Условие перехода в `to`, если оно у этого перехода есть: чего не
    /// хватает, читаемой фразой. Одна функция на весь код — её зовут и
    /// `transition`, и блок задачи в запросе («для перехода не хватает …»).
    /// Таблицу рёбер она не проверяет: это делает `Stage::allowed`.
    pub fn condition(&self, to: Stage) -> Result<(), String> {
        match (self.stage, to) {
            (Stage::Planning, Stage::Execution) => {
                if self.plan.is_empty() {
                    Err("плана нет — агент ещё не выдал блок ```plan".to_string())
                } else if self.plan_approved_at.is_none() {
                    Err("план не утверждён".to_string())
                } else {
                    Ok(())
                }
            }
            (Stage::Validation, Stage::Done) => {
                if self.plan.is_empty() {
                    return Err("плана нет — проверять нечего".to_string());
                }
                let numbers = |ok: Option<bool>| -> Vec<String> {
                    self.plan
                        .iter()
                        .enumerate()
                        .filter(|(_, step)| step.check.as_ref().map(|c| c.ok) == ok)
                        .map(|(i, _)| (i + 1).to_string())
                        .collect()
                };
                let (unchecked, failed) = (numbers(None), numbers(Some(false)));
                let mut missing = Vec::new();
                match unchecked.len() {
                    0 => {}
                    1 => missing.push(format!("не проверен пункт {}", unchecked[0])),
                    _ => missing.push(format!("не проверены пункты {}", unchecked.join(", "))),
                }
                match failed.len() {
                    0 => {}
                    1 => missing.push(format!("пункт {} не прошёл проверку", failed[0])),
                    _ => missing.push(format!("пункты {} не прошли проверку", failed.join(", "))),
                }
                if missing.is_empty() {
                    Ok(())
                } else {
                    Err(missing.join("; "))
                }
            }
            _ => Ok(()),
        }
    }

    /// Перевод этапа: сначала ребро по таблице `Stage::allowed`, потом условие
    /// (`condition`). Отказ этап не меняет, но в журнал попадает с пометкой —
    /// своей для «нет ребра» и для «условие не выполнено»: и предложение
    /// агента, и нажатие человека должны оставлять след.
    ///
    /// Возврат назад сбрасывает то, что стало недействительным: из выполнения
    /// в планирование план снова становится черновиком, из проверки в
    /// выполнение стираются отметки проверки.
    pub fn transition(&mut self, to: Stage, by: &str, note: &str) -> Result<(), String> {
        let from = self.stage;
        let refuse = |task: &mut Task, note: String| {
            task.log.push(Transition { from, to, at: utc_now(), by: by.to_string(), note });
        };
        if !from.allowed().contains(&to) {
            refuse(self, REJECTED_NOTE.to_string());
            let allowed = match from.allowed() {
                [] => "ничего, это конечный этап".to_string(),
                stages => {
                    stages.iter().map(|s| s.label()).collect::<Vec<_>>().join(", ")
                }
            };
            return Err(format!(
                "из «{}» нельзя перейти в «{}»; разрешено: {allowed}",
                from.label(),
                to.label()
            ));
        }
        if let Err(missing) = self.condition(to) {
            refuse(self, format!("{CONDITION_NOTE} — {missing}"));
            return Err(format!("из «{}» в «{}» пока нельзя: {missing}", from.label(), to.label()));
        }
        match (from, to) {
            (Stage::Execution, Stage::Planning) => self.plan_approved_at = None,
            (Stage::Validation, Stage::Execution) => {
                for step in &mut self.plan {
                    step.check = None;
                }
            }
            _ => {}
        }
        self.stage = to;
        self.log.push(Transition {
            from,
            to,
            at: utc_now(),
            by: by.to_string(),
            note: note.to_string(),
        });
        Ok(())
    }

    /// Утверждение плана кнопкой человека: отметка времени и сразу переход
    /// в выполнение. `seen` — шаги карточки, которую человек прочитал: если
    /// черновик с тех пор сменился, утверждать нечего — отказ `STALE_PLAN` и
    /// строка в журнале. Утвердить можно только на планировании; плана нет —
    /// отказ даёт уже `transition`, со своей строкой в журнале.
    pub fn approve(&mut self, seen: &[String]) -> Result<(), String> {
        if self.stage != Stage::Planning {
            return Err(format!(
                "план утверждают на этапе «{}», а задача на этапе «{}»",
                Stage::Planning.label(),
                self.stage.label()
            ));
        }
        if !self.plan.is_empty() && !self.plan.iter().map(|s| &s.text).eq(seen.iter()) {
            self.log.push(Transition {
                from: self.stage,
                to: Stage::Execution,
                at: utc_now(),
                by: "human".to_string(),
                note: format!("{CONDITION_NOTE} — утверждали устаревшую карточку плана"),
            });
            return Err(STALE_PLAN.to_string());
        }
        if !self.plan.is_empty() {
            self.plan_approved_at = Some(utc_now());
        }
        self.transition(Stage::Execution, "human", "план утверждён")
    }

    /// План из ответа агента. Черновик заменяется целиком, но только на
    /// планировании и пока план не утверждён: утверждённый заморожен, а вне
    /// планирования блок ```plan не принимается. Отказ — причина для пометки
    /// под карточкой: «не принят: план уже утверждён» или «не принят: этап …».
    pub fn offer_plan(&mut self, steps: Vec<String>) -> Result<(), String> {
        if self.plan_approved_at.is_some() {
            return Err("не принят: план уже утверждён".to_string());
        }
        if self.stage != Stage::Planning {
            return Err(format!("не принят: этап «{}»", self.stage.label()));
        }
        if steps.is_empty() {
            return Err("не принят: в плане нет шагов".to_string());
        }
        self.plan = steps.into_iter().map(|text| PlanStep { text, check: None }).collect();
        Ok(())
    }

    /// Отметки проверки по пунктам плана: номер с единицы, прошёл ли и
    /// почему. Действуют только на этапе проверки; номер вне плана —
    /// пропускается, как любой мусор из служебного вызова.
    pub fn apply_checks(&mut self, checks: &[(usize, Check)]) {
        if self.stage != Stage::Validation {
            return;
        }
        for (number, check) in checks {
            if let Some(step) = number.checked_sub(1).and_then(|i| self.plan.get_mut(i)) {
                step.check = Some(check.clone());
            }
        }
    }

    /// Пауза: этап, план и контекст остаются на диске как есть. Пока она
    /// стоит, сервер не принимает новые сообщения в чат — это замок, а не
    /// просто отметка; переходы кнопками при этом работают.
    pub fn pause(&mut self) {
        if self.paused_at.is_none() {
            self.paused_at = Some(utc_now());
        }
        self.resumed_from = None;
    }

    /// Продолжение: снимаем паузу и помним, с какого момента её сняли, —
    /// следующий запрос уйдёт со строкой «продолжай с текущего шага».
    pub fn resume(&mut self) {
        if let Some(at) = self.paused_at.take() {
            self.resumed_from = Some(at);
        }
    }
}

/// Рабочая память одной задачи. Один чат — одна задача, поэтому файл
/// адресуется id чата. Счётчики служебных вызовов памяти лежат здесь же:
/// это расход самой памяти, и мешать его с токенами разговора нечестно.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Working {
    #[serde(default)]
    pub facts: Vec<Fact>,
    /// Состояние задачи этого чата. Файлы дня 12 поля не знают — им достаётся
    /// умолчание: этап «планирование», пустые шаг и ожидание.
    #[serde(default)]
    pub task: Task,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub cost: f64,
}

impl Working {
    /// Что переезжает в ветку: факты и состояние задачи вместе с журналом —
    /// ветка продолжает ту же задачу с того же этапа. Счётчики — расход
    /// родителя, и приписывать его ветке значило бы посчитать одни токены
    /// дважды.
    pub fn branched(&self) -> Working {
        Working { facts: self.facts.clone(), task: self.task.clone(), ..Working::default() }
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

/// Категория инварианта. Влияет только на подпись: проверяются все
/// категории одинаково, а разводятся они, чтобы человек видел, какого рода
/// ограничение он задал.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Architecture,
    Decision,
    Stack,
    Business,
}

impl Category {
    pub const ALL: [Category; 4] =
        [Category::Architecture, Category::Decision, Category::Stack, Category::Business];

    pub fn from_id(id: &str) -> Option<Category> {
        Category::ALL.into_iter().find(|c| c.id() == id)
    }

    pub fn id(&self) -> &'static str {
        match self {
            Category::Architecture => "architecture",
            Category::Decision => "decision",
            Category::Stack => "stack",
            Category::Business => "business",
        }
    }

    /// Подпись в интерфейсе и в блоке запроса: `I1 [стек] …`.
    pub fn label(&self) -> &'static str {
        match self {
            Category::Architecture => "архитектура",
            Category::Decision => "решение",
            Category::Stack => "стек",
            Category::Business => "бизнес-правило",
        }
    }
}

/// Инвариант — ограничение проекта, которое ассистент не имеет права
/// нарушить. В отличие от решения в долговременной памяти его пишет только
/// человек, и соблюдение проверяется отдельным вызовом после каждого ответа.
///
/// id короткий и человеческий (`I1`, `I2`…): он стоит в строке сверки и в
/// вердикте валидатора, и модели проще не перепутать `I2`, чем хэш.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Invariant {
    pub id: String,
    pub category: Category,
    pub rule: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    pub created_at: String,
}

fn enabled_by_default() -> bool {
    true
}

/// Набор инвариантов — один на приложение, как и долговременная память.
/// Наборов на чат нет: ограничение проекта не зависит от того, в каком
/// разговоре о нём спросили.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Invariants {
    #[serde(default, deserialize_with = "lenient_invariants")]
    pub items: Vec<Invariant>,
}

/// Та же снисходительность, что у долговременной памяти: запись с
/// неизвестной категорией отбрасывается, остальные читаются.
fn lenient_invariants<'de, D>(deserializer: D) -> Result<Vec<Invariant>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
    Ok(raw.into_iter().filter_map(|item| serde_json::from_value(item).ok()).collect())
}

impl Invariants {
    /// Включённые — именно они идут в запрос и в валидатор.
    pub fn enabled(&self) -> Vec<Invariant> {
        self.items.iter().filter(|i| i.enabled).cloned().collect()
    }

    /// Следующий номер после самого большого из существующих. Удалённый
    /// последний номер может вернуться, но id живых записей не меняются
    /// никогда — на них ссылаются вердикты в старых сообщениях.
    fn next_id(&self) -> String {
        let max = self
            .items
            .iter()
            .filter_map(|i| i.id.strip_prefix('I').and_then(|n| n.parse::<u32>().ok()))
            .max()
            .unwrap_or(0);
        format!("I{}", max + 1)
    }

    /// Новый инвариант, включённый. Пустое правило — отказ с причиной: без
    /// текста проверять нечего.
    pub fn add(&mut self, category: Category, rule: &str, reason: &str) -> Result<Invariant, String> {
        let rule = rule.trim();
        if rule.is_empty() {
            return Err("у инварианта должно быть правило".to_string());
        }
        let invariant = Invariant {
            id: self.next_id(),
            category,
            rule: rule.to_string(),
            reason: reason.trim().to_string(),
            enabled: true,
            created_at: utc_now(),
        };
        self.items.push(invariant.clone());
        Ok(invariant)
    }

    /// Правка по id. Поля, которых нет, остаются как были: тумблер и форма
    /// ходят одним маршрутом.
    pub fn edit(
        &mut self,
        id: &str,
        category: Option<Category>,
        rule: Option<&str>,
        reason: Option<&str>,
        enabled: Option<bool>,
    ) -> Result<(), String> {
        let rule = rule.map(str::trim);
        if rule.is_some_and(str::is_empty) {
            return Err("у инварианта должно быть правило".to_string());
        }
        let item = self.items.iter_mut().find(|i| i.id == id).ok_or("такого инварианта нет")?;
        if let Some(category) = category {
            item.category = category;
        }
        if let Some(rule) = rule {
            item.rule = rule.to_string();
        }
        if let Some(reason) = reason {
            item.reason = reason.trim().to_string();
        }
        if let Some(enabled) = enabled {
            item.enabled = enabled;
        }
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> Result<(), String> {
        let before = self.items.len();
        self.items.retain(|i| i.id != id);
        if self.items.len() == before {
            return Err("такого инварианта нет".to_string());
        }
        Ok(())
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

/// Указатели приложения: открытый чат. Поле `active_profile` из дней 12–14
/// в старом файле просто игнорируется.
#[derive(Serialize, Deserialize, Default)]
struct StateFile {
    #[serde(default)]
    active_chat: Option<String>,
}

pub struct Store {
    root: PathBuf,
    chats: PathBuf,
    working: PathBuf,
}

impl Store {
    /// Создаёт папки и сразу читает всё, что там лежит: битые файлы должны
    /// обнаружиться на старте, а не при первом клике.
    pub fn open(root: impl AsRef<Path>) -> Result<Store, String> {
        let root = root.as_ref().to_path_buf();
        let chats = root.join("chats");
        let working = root.join("working");
        for dir in [&chats, &working] {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("не удалось создать {}: {e}", dir.display()))?;
        }
        let store = Store { root, chats, working };
        let _ = store.list();
        store.seed_instructions()?;
        store.seed_invariants()?;
        Ok(store)
    }

    /// Файла инвариантов нет — сеем четыре демонстрационных. Файл есть, пусть
    /// и пустой, — ничего не трогаем: человек мог удалить их все нарочно.
    fn seed_invariants(&self) -> Result<(), String> {
        if self.invariants_path().exists() {
            return Ok(());
        }
        let mut invariants = Invariants::default();
        for (category, rule, reason) in DEMO_INVARIANTS {
            invariants.add(*category, rule, reason)?;
        }
        self.save_invariants(&invariants)
    }

    /// Инварианты — один файл на приложение. Битый файл даёт пустой набор и
    /// строку в stderr, как у долговременной памяти.
    pub fn invariants(&self) -> Invariants {
        let path = self.invariants_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                eprintln!("не читается {}: {e}", path.display());
                Invariants::default()
            }),
            Err(_) => Invariants::default(),
        }
    }

    pub fn save_invariants(&self, invariants: &Invariants) -> Result<(), String> {
        let text = serde_json::to_string_pretty(invariants)
            .map_err(|e| format!("не сериализуется: {e}"))?;
        write_atomic(&self.invariants_path(), &text)
    }

    fn invariants_path(&self) -> PathBuf {
        self.root.join("invariants.json")
    }

    /// Файла инструкций нет — сеем текст по умолчанию. Файл есть, пусть и
    /// пустой, — не трогаем: человек мог стереть инструкции нарочно.
    fn seed_instructions(&self) -> Result<(), String> {
        if self.instructions_path().exists() {
            return Ok(());
        }
        self.save_instructions(DEFAULT_INSTRUCTIONS)
    }

    /// Инструкции — обычный текстовый файл: его удобно открыть и поправить
    /// руками. Нечитаемый файл даёт пустой текст и строку в stderr.
    pub fn instructions(&self) -> String {
        let path = self.instructions_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                if path.exists() {
                    eprintln!("не читается {}: {e}", path.display());
                }
                String::new()
            }
        }
    }

    pub fn save_instructions(&self, text: &str) -> Result<(), String> {
        write_atomic(&self.instructions_path(), text)
    }

    fn instructions_path(&self) -> PathBuf {
        self.root.join("instructions.md")
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
            let state = StateFile { active_chat: next };
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
        let state = StateFile { active_chat: Some(id.to_string()) };
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
        let dir = std::env::temp_dir().join(format!("w3d5-{name}-{}-{}", std::process::id(), new_id()));
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
                    task: Task {
                        stage: Stage::Execution,
                        step: "выбираем платформу".to_string(),
                        expected: "человек подтверждает вариант".to_string(),
                        log: vec![Transition {
                            from: Stage::Planning,
                            to: Stage::Execution,
                            at: utc_now(),
                            by: "agent".to_string(),
                            note: "требования собраны".to_string(),
                        }],
                        ..Task::default()
                    },
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
        // Ветка продолжает ту же задачу: этап, шаг и журнал переезжают вместе
        // с фактами — иначе продолжение начиналось бы с планирования.
        assert_eq!(copied.task.stage, Stage::Execution);
        assert_eq!(copied.task.step, "выбираем платформу");
        assert_eq!(copied.task.log.len(), 1);
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
        // Тип `profile` был в дне 11 и остался только в старых файлах.
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

    /// Нет файла — засеваются инструкции по умолчанию. Файл есть, даже
    /// пустой, — посев не повторяется, и текст переживает перезапуск.
    #[test]
    fn instructions_are_seeded_once_and_survive_a_restart() {
        let (dir, store) = temp_store("instructions");
        assert!(dir.join("instructions.md").exists());
        assert_eq!(store.instructions(), DEFAULT_INSTRUCTIONS);
        assert!(store.instructions().starts_with("Я изучаю робототехнику и Rust."));

        store.save_instructions("Отвечай стихами").unwrap();
        let again = Store::open(&dir).expect("папка открывается второй раз");
        assert_eq!(again.instructions(), "Отвечай стихами");

        // Человек стёр всё — второй запуск умолчание не возвращает.
        again.save_instructions("").unwrap();
        let third = Store::open(&dir).expect("папка открывается третий раз");
        assert_eq!(third.instructions(), "");

        // Старый state.json с указателем на профиль читается и не мешает.
        std::fs::write(dir.join("state.json"), r#"{"active_chat":null,"active_profile":"abc"}"#).unwrap();
        assert_eq!(third.active(), None);

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

    /// Задача с планом из двух пунктов, уже утверждённым: все условия, кроме
    /// отметок проверки, выполнены — удобно гонять таблицу рёбер.
    fn approved_task() -> Task {
        let mut task = Task::default();
        task.offer_plan(vec!["собрать шасси".to_string(), "прошить".to_string()]).unwrap();
        task.plan_approved_at = Some(utc_now());
        task
    }

    fn passed(note: &str) -> Option<Check> {
        Some(Check { ok: true, note: note.to_string() })
    }

    /// Таблица переходов — единственное место, где написано, куда можно
    /// двигаться. Она в коде, а не в промпте: промпт теряется при
    /// суммаризации и правится руками, код — нет.
    #[test]
    fn the_transition_table_allows_only_the_listed_moves() {
        let mut task = approved_task();
        assert_eq!(task.stage, Stage::Planning, "новая задача начинается с планирования");

        // Перепрыгнуть через этап нельзя, и этап от такой попытки не двигается.
        let error = task.transition(Stage::Done, "human", "кнопка").unwrap_err();
        assert_eq!(error, "из «планирование» нельзя перейти в «готово»; разрешено: выполнение");
        assert_eq!(task.stage, Stage::Planning);

        task.transition(Stage::Execution, "human", "кнопка").expect("планирование → выполнение");
        assert_eq!(task.stage, Stage::Execution);

        // Назад можно: требования всплывают поздно.
        task.transition(Stage::Planning, "human", "всплыло требование").expect("назад разрешено");
        assert_eq!(task.stage, Stage::Planning);

        task.plan_approved_at = Some(utc_now());
        task.transition(Stage::Execution, "human", "кнопка").unwrap();
        task.transition(Stage::Validation, "human", "кнопка").unwrap();
        for step in &mut task.plan {
            step.check = passed("");
        }
        task.transition(Stage::Done, "human", "кнопка").unwrap();
        assert_eq!(task.stage, Stage::Done);

        // `done` конечный: из него не выйти никуда.
        for stage in Stage::ALL {
            let error = task.transition(stage, "human", "кнопка").unwrap_err();
            assert!(error.ends_with("разрешено: ничего, это конечный этап"), "{error}");
        }
        assert_eq!(task.stage, Stage::Done);
    }

    /// Журнал пишется и на удавшийся переход, и на отклонённый: по нему должно
    /// быть видно, что агент рвался вперёд, а код его не пустил.
    #[test]
    fn the_log_keeps_both_moves_and_refusals() {
        let mut task = approved_task();
        task.transition(Stage::Execution, "agent", "требования собраны").expect("разрешено");
        assert!(task.transition(Stage::Done, "agent", "хочу закрыть").is_err());

        assert_eq!(task.log.len(), 2, "{:?}", task.log);
        let done_move = &task.log[0];
        assert_eq!((done_move.from, done_move.to), (Stage::Planning, Stage::Execution));
        assert_eq!(done_move.by, "agent");
        assert_eq!(done_move.note, "требования собраны");
        assert!(done_move.at.ends_with('Z'), "{}", done_move.at);

        let refused = &task.log[1];
        assert_eq!((refused.from, refused.to), (Stage::Execution, Stage::Done));
        assert_eq!(refused.note, REJECTED_NOTE);
        assert_eq!(task.stage, Stage::Execution, "отклонённая попытка этап не двигает");
    }

    /// Пауза — это отметка «мы вышли», а не замок: запросы она не блокирует.
    /// Продолжение оставляет флаг на один следующий запрос.
    #[test]
    fn a_pause_keeps_the_stage_and_resume_leaves_a_flag() {
        let mut task = Task { stage: Stage::Execution, step: "шаг".to_string(), ..Task::default() };
        task.pause();
        let paused_at = task.paused_at.clone().expect("пауза отмечена");
        assert_eq!(task.stage, Stage::Execution, "пауза этап не двигает");
        assert_eq!(task.resumed_from, None);

        // Вторая пауза подряд время не переписывает.
        task.pause();
        assert_eq!(task.paused_at.as_deref(), Some(paused_at.as_str()));

        task.resume();
        assert_eq!(task.paused_at, None);
        assert_eq!(task.resumed_from.as_deref(), Some(paused_at.as_str()));
        assert_eq!(task.step, "шаг", "продолжение шаг не теряет");
    }

    /// Задача лежит в рабочей памяти чата, поэтому переживает перезапуск
    /// сервера так же, как факты: это и есть «пауза на любом этапе».
    #[test]
    fn a_task_survives_a_round_trip_through_a_file() {
        let (dir, store) = temp_store("task");
        let chat = Chat::new(Settings::default());
        store.save(&chat).unwrap();

        let mut working = Working { facts: vec![fact("бюджет", "400 тысяч")], ..Working::default() };
        let steps = vec!["собрать шасси".to_string(), "прошить".to_string()];
        working.task.offer_plan(steps.clone()).unwrap();
        working.task.approve(&steps).expect("план есть — утверждается");
        working.task.transition(Stage::Validation, "agent", "шаги выполнены").unwrap();
        working.task.plan[0].check = passed("собрано");
        working.task.step = "выбираем платформу".to_string();
        working.task.expected = "человек подтверждает вариант".to_string();
        working.task.pause();
        store.save_working(&chat.id, &working).unwrap();

        // Свежий Store — это и есть перезапуск сервера.
        let reopened = Store::open(&dir).expect("папка открывается повторно");
        let back = reopened.working(&chat.id);
        assert_eq!(back.task.stage, Stage::Validation);
        assert_eq!(back.task.step, "выбираем платформу");
        assert_eq!(back.task.expected, "человек подтверждает вариант");
        assert!(back.task.paused_at.is_some(), "пауза тоже на диске");
        assert_eq!(back.task.log.len(), 2);
        // План, его утверждение и отметки проверки переживают перезапуск —
        // и условие перехода считается по ним так же, как до него.
        assert_eq!(back.task, working.task);
        assert!(back.task.plan_approved_at.is_some());
        assert_eq!(back.task.plan[0].check, passed("собрано"));
        assert_eq!(back.task.plan[1].check, None);
        assert_eq!(back.task.condition(Stage::Done), Err("не проверен пункт 2".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Рабочая память дня 12 поля `task` не знает — такой файл должен читаться
    /// дальше и начинать задачу с планирования.
    #[test]
    fn working_memory_without_a_task_still_loads() {
        let json = r#"{"facts":[{"key":"бюджет","value":"400 тысяч"}],
            "prompt_tokens":300,"completion_tokens":40,"cost":0.02}"#;
        let working: Working = serde_json::from_str(json).expect("старый файл читается");
        assert_eq!(working.facts.len(), 1);
        assert_eq!(working.task.stage, Stage::Planning);
        assert!(working.task.step.is_empty() && working.task.expected.is_empty());
        assert!(working.task.log.is_empty());
        assert_eq!(working.task.paused_at, None);

        // Задача дня 13 — с этапом, но без плана: читается, план пустой и не
        // утверждён, а переход дальше честно говорит, чего не хватает.
        let json = r#"{"facts":[],"task":{"stage":"execution","step":"шаг","log":[]}}"#;
        let working: Working = serde_json::from_str(json).expect("файл дня 13 читается");
        assert_eq!(working.task.stage, Stage::Execution);
        assert!(working.task.plan.is_empty() && working.task.plan_approved_at.is_none());
    }

    /// Условия переходов: ребро в таблице есть, а пускает только выполненное
    /// условие. Отказ пишется в журнал своей пометкой — с тем, чего не хватило.
    #[test]
    fn transitions_are_guarded_by_their_conditions() {
        let mut task = Task::default();
        let error = task.transition(Stage::Execution, "agent", "пора").unwrap_err();
        assert_eq!(error, "из «планирование» в «выполнение» пока нельзя: плана нет — агент ещё не выдал блок ```plan");

        let steps = vec!["собрать шасси".to_string(), "прошить".to_string(), "проверить ход".to_string()];
        task.offer_plan(steps.clone()).unwrap();
        let error = task.transition(Stage::Execution, "human", "кнопка").unwrap_err();
        assert!(error.ends_with("план не утверждён"), "{error}");
        assert_eq!(task.stage, Stage::Planning);
        assert_eq!(task.log.len(), 2);
        assert_eq!(task.log[1].note, format!("{CONDITION_NOTE} — план не утверждён"));
        assert_ne!(task.log[1].note, REJECTED_NOTE, "нет условия и нет ребра — разные пометки");

        // Утверждение — отметка и переход одним действием.
        task.approve(&steps).expect("план есть");
        assert_eq!(task.stage, Stage::Execution);
        assert_eq!(task.log.last().map(|t| (t.by.as_str(), t.note.as_str())), Some(("human", "план утверждён")));
        // Утверждённый план заморожен: новый блок ```plan его не заменяет.
        assert_eq!(task.offer_plan(vec!["другой план".to_string()]), Err("не принят: план уже утверждён".to_string()));
        assert_eq!(task.plan.len(), 3);

        task.transition(Stage::Validation, "human", "кнопка").unwrap();
        let error = task.transition(Stage::Done, "agent", "всё готово").unwrap_err();
        assert!(error.ends_with("не проверены пункты 1, 2, 3"), "{error}");

        task.apply_checks(&[(1, Check { ok: true, note: "собрано".to_string() }),
                            (3, Check { ok: false, note: "колесо трёт".to_string() }),
                            (9, Check { ok: true, note: "нет такого".to_string() })]);
        assert_eq!(
            task.condition(Stage::Done),
            Err("не проверен пункт 2; пункт 3 не прошёл проверку".to_string())
        );
        let error = task.transition(Stage::Done, "human", "кнопка").unwrap_err();
        assert!(error.ends_with("не проверен пункт 2; пункт 3 не прошёл проверку"), "{error}");
        assert_eq!(task.stage, Stage::Validation);

        // Назад в выполнение: отметки проверки стираются — после доработки
        // проверять придётся заново.
        task.transition(Stage::Execution, "human", "чиним колесо").unwrap();
        assert!(task.plan.iter().all(|s| s.check.is_none()));
        // Назад в планирование: план снова черновик, его можно заменить.
        task.transition(Stage::Planning, "human", "всплыло требование").unwrap();
        assert_eq!(task.plan_approved_at, None);
        task.offer_plan(vec!["новый шаг".to_string()]).unwrap();
        assert_eq!(task.plan.len(), 1);

        // Утвердить можно только на планировании.
        task.approve(&["новый шаг".to_string()]).unwrap();
        assert!(task.approve(&[]).unwrap_err().contains("а задача на этапе «выполнение»"));
        // И только когда план есть: иначе отказ условием, со строкой в журнале.
        let mut empty = Task::default();
        assert!(empty.approve(&[]).unwrap_err().ends_with("плана нет — агент ещё не выдал блок ```plan"));
        assert_eq!(empty.plan_approved_at, None);
        assert_eq!(empty.log.len(), 1);
        // Вне планирования блок плана игнорируется.
        let mut running = Task { stage: Stage::Execution, ..Task::default() };
        assert_eq!(running.offer_plan(vec!["шаг".to_string()]), Err("не принят: этап «выполнение»".to_string()));
        assert!(running.plan.is_empty());
    }

    /// Утверждается ровно то, что человек прочитал: черновик сменился, пока
    /// он смотрел на старую карточку, — отказ «план изменился», строка в
    /// журнале, и этап стоит на месте. Свежая карточка утверждается.
    #[test]
    fn approving_a_stale_plan_card_is_refused_and_logged() {
        let mut task = Task::default();
        let old = vec!["собрать шасси".to_string()];
        let new = vec!["собрать шасси".to_string(), "проверить питание".to_string()];
        task.offer_plan(old.clone()).unwrap();
        task.offer_plan(new.clone()).unwrap();

        assert_eq!(task.approve(&old), Err(STALE_PLAN.to_string()));
        assert_eq!(task.stage, Stage::Planning);
        assert_eq!(task.plan_approved_at, None, "устаревшая карточка ничего не утвердила");
        assert_eq!(task.log.len(), 1);
        assert_eq!(task.log[0].note, format!("{CONDITION_NOTE} — утверждали устаревшую карточку плана"));
        // Пустая карточка при непустом черновике — тоже устаревшая.
        assert_eq!(task.approve(&[]), Err(STALE_PLAN.to_string()));

        task.approve(&new).expect("карточка совпадает с черновиком");
        assert_eq!(task.stage, Stage::Execution);
        assert!(task.plan_approved_at.is_some());
    }

    /// Красный путь перебором: все 16 пар «откуда → куда» на всех состояниях
    /// условий — плана нет / черновик / утверждён, отметок нет / часть /
    /// все ✓ / есть ✗. Переход применяется ровно тогда, когда есть ребро и
    /// выполнено условие; иначе этап не меняется, план не трогается, а в
    /// журнале ровно одна запись с правильной пометкой. Ожидания — из
    /// определения дня, а не из тех же `allowed` и `condition`.
    #[test]
    fn every_move_on_every_condition_state_obeys_the_edge_and_the_condition() {
        #[derive(Clone, Copy, Debug, PartialEq)]
        enum PlanState { NoPlan, Draft, Approved }
        #[derive(Clone, Copy, Debug, PartialEq)]
        enum Marks { Unchecked, Partial, AllOk, HasFailed }
        use Stage::{Done, Execution, Planning, Validation};

        let mut cases = 0;
        for from in Stage::ALL {
            for to in Stage::ALL {
                for plan in [PlanState::NoPlan, PlanState::Draft, PlanState::Approved] {
                    let variants: &[Marks] = if plan == PlanState::NoPlan {
                        &[Marks::Unchecked]
                    } else {
                        &[Marks::Unchecked, Marks::Partial, Marks::AllOk, Marks::HasFailed]
                    };
                    for &marks in variants {
                        let mut task = Task { stage: from, ..Task::default() };
                        if plan != PlanState::NoPlan {
                            task.plan = ["собрать шасси", "прошить", "проверить ход"]
                                .map(|text| PlanStep { text: text.to_string(), check: None })
                                .to_vec();
                        }
                        if plan == PlanState::Approved {
                            task.plan_approved_at = Some(utc_now());
                        }
                        let oks: [Option<bool>; 3] = match marks {
                            Marks::Unchecked => [None; 3],
                            Marks::Partial => [Some(true), None, None],
                            Marks::AllOk => [Some(true); 3],
                            Marks::HasFailed => [Some(true), Some(true), Some(false)],
                        };
                        for (step, ok) in task.plan.iter_mut().zip(oks) {
                            step.check = ok.map(|ok| Check { ok, note: String::new() });
                        }
                        let before = task.clone();

                        let edge = matches!(
                            (from, to),
                            (Planning, Execution)
                                | (Execution, Validation)
                                | (Execution, Planning)
                                | (Validation, Done)
                                | (Validation, Execution)
                        );
                        let condition = match (from, to) {
                            (Planning, Execution) => plan == PlanState::Approved,
                            (Validation, Done) => plan != PlanState::NoPlan && marks == Marks::AllOk,
                            _ => true,
                        };
                        let case = format!("{from:?} → {to:?}, план {plan:?}, отметки {marks:?}");
                        let result = task.transition(to, "human", "кнопка");

                        assert_eq!(task.log.len(), 1, "ровно одна запись: {case}");
                        if edge && condition {
                            assert!(result.is_ok(), "{case}: {result:?}");
                            assert_eq!(task.stage, to, "{case}");
                            assert_eq!(task.log[0].note, "кнопка", "{case}");
                            match (from, to) {
                                (Execution, Planning) => {
                                    assert_eq!(task.plan_approved_at, None, "назад — план снова черновик: {case}");
                                    assert_eq!(task.plan, before.plan, "{case}");
                                }
                                (Validation, Execution) => {
                                    assert!(task.plan.iter().all(|s| s.check.is_none()), "назад — отметки стёрты: {case}");
                                    assert_eq!(task.plan_approved_at, before.plan_approved_at, "{case}");
                                }
                                _ => {
                                    assert_eq!(task.plan, before.plan, "{case}");
                                    assert_eq!(task.plan_approved_at, before.plan_approved_at, "{case}");
                                }
                            }
                        } else {
                            assert!(result.is_err(), "{case}");
                            assert_eq!(task.stage, from, "отказ этап не двигает: {case}");
                            assert_eq!(task.plan, before.plan, "{case}");
                            assert_eq!(task.plan_approved_at, before.plan_approved_at, "{case}");
                            let note = &task.log[0].note;
                            if edge {
                                assert!(note.starts_with(&format!("{CONDITION_NOTE} — ")), "{case}: {note}");
                            } else {
                                assert_eq!(note, REJECTED_NOTE, "{case}");
                            }
                        }
                        cases += 1;
                    }
                }
            }
        }
        assert_eq!(cases, 16 * 9, "все пары на всех состояниях");
    }

    /// Красный путь последовательностями: все цепочки до четырёх событий из
    /// набора «агент выдал план, человек утвердил, переход в любой этап от
    /// агента или человека, отметки все ✓ или с ✗, пауза, продолжение» от
    /// чистой задачи. Что бы ни случилось и в каком порядке: выполнение,
    /// проверка и «готово» не наступают без утверждённого плана, «готово» —
    /// без всех ✓, а пауза и продолжение не трогают этап, план и отметки.
    /// «Готово» от чистой задачи за четыре события не достать, поэтому тот же
    /// перебор идёт ещё и от задачи, честно доведённой до проверки.
    #[test]
    fn no_chain_of_four_events_breaks_the_automaton() {
        #[derive(Clone, Copy, Debug)]
        enum Act { Offer, Approve, Move(Stage, &'static str), ChecksOk, ChecksFailed, Pause, Resume }

        fn apply(task: &mut Task, act: Act) {
            let steps = vec!["собрать шасси".to_string(), "прошить".to_string()];
            let mark = |ok: bool| Check { ok, note: String::new() };
            match act {
                Act::Offer => {
                    let _ = task.offer_plan(steps);
                }
                Act::Approve => {
                    let _ = task.approve(&steps);
                }
                Act::Move(to, by) => {
                    let _ = task.transition(to, by, "ход");
                }
                Act::ChecksOk => task.apply_checks(&[(1, mark(true)), (2, mark(true))]),
                Act::ChecksFailed => task.apply_checks(&[(1, mark(true)), (2, mark(false))]),
                Act::Pause => task.pause(),
                Act::Resume => task.resume(),
            }
        }

        fn walk(
            task: &Task,
            acts: &[Act],
            path: &mut Vec<Act>,
            depth: usize,
            count: &mut usize,
            reached: &mut Vec<Stage>,
        ) {
            if depth == 0 {
                return;
            }
            for &act in acts {
                let mut next = task.clone();
                apply(&mut next, act);
                path.push(act);
                *count += 1;
                if !reached.contains(&next.stage) {
                    reached.push(next.stage);
                }
                if matches!(next.stage, Stage::Execution | Stage::Validation | Stage::Done) {
                    assert!(
                        next.plan_approved_at.is_some() && !next.plan.is_empty(),
                        "этап {:?} без утверждённого плана: {path:?}",
                        next.stage
                    );
                }
                if next.stage == Stage::Done {
                    assert!(
                        next.plan.iter().all(|s| s.check.as_ref().is_some_and(|c| c.ok)),
                        "«готово» без всех ✓: {path:?}"
                    );
                }
                if matches!(act, Act::Pause | Act::Resume) {
                    assert_eq!(next.stage, task.stage, "{path:?}");
                    assert_eq!(next.plan, task.plan, "{path:?}");
                    assert_eq!(next.plan_approved_at, task.plan_approved_at, "{path:?}");
                    assert_eq!(next.log, task.log, "пауза в журнал переходов не пишет: {path:?}");
                }
                assert!(next.log.len() <= task.log.len() + 1, "одно событие — не больше одной записи: {path:?}");
                walk(&next, acts, path, depth - 1, count, reached);
                path.pop();
            }
        }

        let mut acts = vec![Act::Offer, Act::Approve, Act::ChecksOk, Act::ChecksFailed, Act::Pause, Act::Resume];
        for to in Stage::ALL {
            acts.push(Act::Move(to, "agent"));
            acts.push(Act::Move(to, "human"));
        }
        let all_chains = 14 + 14 * 14 + 14usize.pow(3) + 14usize.pow(4);
        let (mut count, mut reached) = (0, Vec::new());
        walk(&Task::default(), &acts, &mut Vec::new(), 4, &mut count, &mut reached);
        assert_eq!(count, all_chains, "перебраны все цепочки");
        assert!(reached.contains(&Stage::Validation), "перебор доходит до проверки: {reached:?}");

        let mut checking = Task::default();
        for act in [Act::Offer, Act::Approve, Act::Move(Stage::Validation, "human")] {
            apply(&mut checking, act);
        }
        assert_eq!(checking.stage, Stage::Validation);
        let (mut count, mut reached) = (0, Vec::new());
        walk(&checking, &acts, &mut Vec::new(), 4, &mut count, &mut reached);
        assert_eq!(count, all_chains);
        assert!(reached.contains(&Stage::Done), "с проверки перебор доходит до «готово»: {reached:?}");
    }

    /// Нет файла — засеваются четыре демо-инварианта с номерами по порядку.
    /// Файл есть, даже пустой, — посев не повторяется.
    #[test]
    fn a_fresh_folder_gets_four_demo_invariants_once() {
        let (dir, store) = temp_store("invseed");
        let set = store.invariants();
        let ids: Vec<&str> = set.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["I1", "I2", "I3", "I4"]);
        let categories: Vec<Category> = set.items.iter().map(|i| i.category).collect();
        assert_eq!(
            categories,
            [Category::Stack, Category::Architecture, Category::Decision, Category::Business]
        );
        assert!(set.items[1].rule.contains("Raspberry Pi"), "{:?}", set.items[1]);
        assert!(set.items.iter().all(|i| i.enabled && !i.reason.is_empty()));
        assert_eq!(set.enabled().len(), 4);

        // Человек удалил всё — второй запуск ничего не возвращает.
        store.save_invariants(&Invariants::default()).unwrap();
        let again = Store::open(&dir).expect("папка открывается второй раз");
        assert!(again.invariants().items.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invariants_are_edited_only_by_id_and_survive_the_file() {
        let (dir, store) = temp_store("invcrud");
        let mut set = store.invariants();

        assert_eq!(
            set.add(Category::Stack, "   ", "причина").unwrap_err(),
            "у инварианта должно быть правило"
        );
        let added = set.add(Category::Decision, "  Моторы — только N20  ", "  склад кружка ").unwrap();
        assert_eq!(added.id, "I5");
        assert_eq!(added.rule, "Моторы — только N20", "края обрезаются");
        assert_eq!(added.reason, "склад кружка");

        // Тумблер трогает только `enabled`, форма — остальное.
        set.edit("I2", None, None, None, Some(false)).unwrap();
        set.edit("I5", Some(Category::Stack), Some("Моторы — N20 или TT"), Some(""), None).unwrap();
        assert!(set.edit("I5", None, Some(" "), None, None).is_err(), "пустое правило — отказ");
        assert!(set.edit("I42", None, None, None, Some(true)).is_err());
        assert_eq!(set.enabled().iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["I1", "I3", "I4", "I5"]);

        // Удаление середины номера не сдвигает; следующий — после максимума.
        set.remove("I3").unwrap();
        assert!(set.remove("I3").is_err());
        assert_eq!(set.add(Category::Business, "Без пайки на уроке", "").unwrap().id, "I6");

        store.save_invariants(&set).unwrap();
        let back = store.invariants();
        assert_eq!(back.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["I1", "I2", "I4", "I5", "I6"]);
        let edited = back.items.iter().find(|i| i.id == "I5").unwrap();
        assert_eq!((edited.category, edited.rule.as_str(), edited.reason.as_str()), (Category::Stack, "Моторы — N20 или TT", ""));
        assert!(!back.items[1].enabled);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Запись с неизвестной категорией отбрасывается, остальные читаются; без
    /// поля `enabled` инвариант включён.
    #[test]
    fn broken_invariants_do_not_drop_the_whole_set() {
        let (dir, store) = temp_store("invold");
        let raw = r#"{"items":[
            {"id":"I1","category":"safety","rule":"x","created_at":"2026-09-17T00:00:00Z"},
            {"id":"I2","category":"stack","rule":"Только Rust","created_at":"2026-09-17T00:00:00Z"}]}"#;
        std::fs::write(dir.join("invariants.json"), raw).unwrap();
        let set = store.invariants();
        assert_eq!(set.items.len(), 1, "{:?}", set.items);
        assert_eq!(set.items[0].id, "I2");
        assert!(set.items[0].enabled);
        assert_eq!(set.items[0].reason, "");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Вердикт лежит у ответа и переживает файл; у ответов дня 13 его нет, и
    /// они открываются как раньше.
    #[test]
    fn a_verdict_is_kept_with_the_answer_and_old_answers_have_none() {
        use crate::agent::{Attempt, Verdict, VerdictStatus, Violation};
        let json = r#"{"id":"abc124","title":"Чат дня 13",
            "created_at":"2026-09-16T10:00:00Z","updated_at":"2026-09-16T10:00:00Z",
            "settings":{"provider":"cerebras","model":"qwen-3.8-27b","temperature":0.7,
                "reasoning":"none","persona":"free","system_prompt":"промпт",
                "layers":{"profile":true,"long_term":true,"task":true,"working":true}},
            "messages":[{"role":"user","content":"вопрос"},
                        {"role":"assistant","content":"ответ","metrics":null}]}"#;
        let mut chat: Chat = serde_json::from_str(json).expect("файл дня 13 читается");
        assert!(chat.messages[1].verdict.is_none());
        assert!(chat.settings.layers.invariants, "тумблера не было — слой включён");
        let plain = serde_json::to_string(&chat.messages[1]).unwrap();
        assert!(!plain.contains("verdict"), "ответ без проверки не распухает: {plain}");

        let verdict = Verdict {
            status: VerdictStatus::Fixed,
            checked: vec!["I1".to_string(), "I2".to_string()],
            attempts: vec![
                Attempt {
                    violations: vec![Violation {
                        id: "I2".to_string(),
                        quote: "Raspberry Pi".to_string(),
                        why: "одноплатник".to_string(),
                    }],
                    error: None,
                },
                Attempt::default(),
            ],
            rejected_draft: Some("возьми Raspberry Pi".to_string()),
            note: None,
        };
        chat.messages[1].verdict = Some(verdict.clone());
        let back: Chat = serde_json::from_str(&serde_json::to_string(&chat).unwrap()).unwrap();
        assert_eq!(back.messages[1].verdict, Some(verdict));
    }
}
