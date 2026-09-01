//! Пошаговый мастер настройки: где есть выбор — стрелки, где нужен ввод — ввод.
//!
//! Мастер спрашивает только то, что имеет смысл при уже сделанном выборе:
//! у свободного формата нет схемы, у диалога нет числа прогонов.

use std::error::Error;

use dialoguer::theme::ColorfulTheme;
use dialoguer::{Completion, Confirm, Input, Select};

use crate::format::{self, Constraints, OutputFormat};
use crate::ui;

/// Tab подставляет прошлое значение: набирать заново длинный запрос
/// или список полей неудобно, а править предыдущий — обычное дело.
struct Previous(String);

impl Completion for Previous {
    fn get(&self, _input: &str) -> Option<String> {
        (!self.0.is_empty()).then(|| self.0.clone())
    }
}

/// Ввод строки в том же стиле, что и в мастере.
///
/// Читать stdin напрямую нельзя: после Select/Input терминал остаётся в
/// raw-режиме, и обычный read_line показывает ^M вместо Enter и ^? вместо
/// Backspace — печатать ответ невозможно.
pub fn ask_line(prompt: &str) -> Result<Option<String>, Box<dyn Error>> {
    interrupted(
        Input::<String>::with_theme(&ColorfulTheme::default())
            .with_prompt(prompt)
            .allow_empty(true)
            .interact_text(),
    )
}

/// Что делать после того, как ответ показан.
pub enum Next {
    Again,
    Reconfigure,
    Quit,
}

/// Прерывание (Ctrl-C, Esc, конец потока ввода) — это выход, а не ошибка.
pub fn interrupted<T>(result: dialoguer::Result<T>) -> Result<Option<T>, Box<dyn Error>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(dialoguer::Error::IO(err))
            if matches!(
                err.kind(),
                std::io::ErrorKind::Interrupted | std::io::ErrorKind::UnexpectedEof
            ) =>
        {
            Ok(None)
        }
        // Запуск из пайпа или без tty: мастеру нужны живые клавиши.
        Err(dialoguer::Error::IO(err)) if err.kind() == std::io::ErrorKind::NotConnected => {
            ui::note("ввод не из терминала — мастеру нужен интерактивный запуск");
            Ok(None)
        }
        Err(err) => Err(err.into()),
    }
}

macro_rules! step {
    ($expr:expr) => {
        match interrupted($expr)? {
            Some(value) => value,
            None => return Ok(None),
        }
    };
}

/// Проводит по всем шагам. `None` — пользователь прервал настройку.
pub fn configure(
    question: &str,
    current: &Constraints,
) -> Result<Option<(String, Constraints)>, Box<dyn Error>> {
    let theme = ColorfulTheme::default();

    // Пока запроса нет, подставлять нечего. Дальше прошлый запрос достаётся
    // по Tab — так его видно только когда он нужен, и он не мешает вводить новый.
    let previous_question = Previous(question.to_string());
    let mut input = Input::<String>::with_theme(&theme);
    input = if question.is_empty() {
        input.with_prompt("Запрос")
    } else {
        input
            .with_prompt("Запрос (Tab — прошлый)")
            .completion_with(&previous_question)
    };
    let question: String = step!(input.interact_text());

    let modes = [
        "Одиночный запрос (сравнить с ответом без ограничений)",
        "Диалог: модель уточняет, пока не хватит данных",
    ];
    let dialog = step!(Select::with_theme(&theme)
        .with_prompt("Режим")
        .items(modes)
        .default(usize::from(current.dialog))
        .interact())
        == 1;

    // В диалоге ответ всегда JSON-обёртка {status, question, result}: на ней
    // держится решение, продолжать разговор или заканчивать. Спрашивать формат
    // там нечего — markdown-таблица не может нести status.
    let format = if dialog {
        OutputFormat::Json
    } else {
        let formats = [
            "JSON с заданными полями",
            "Markdown-таблица",
            "Свободный (без описания формата)",
        ];
        match step!(Select::with_theme(&theme)
            .with_prompt("Формат ответа")
            .items(formats)
            .default(match current.format {
                OutputFormat::Json => 0,
                OutputFormat::Markdown => 1,
                OutputFormat::Free => 2,
            })
            .interact())
        {
            0 => OutputFormat::Json,
            1 => OutputFormat::Markdown,
            _ => OutputFormat::Free,
        }
    };

    // Поля нужны только там, где есть что проверять.
    let previous_fields = Previous(current.fields.join(" "));
    let fields = if format == OutputFormat::Free {
        current.fields.clone()
    } else {
        loop {
            // Один и тот же список играет разные роли: в JSON это имена полей,
            // в таблице — заголовки колонок. Спрашивать надо тем же словом.
            // Список полей — это и есть контракт ответа: чего в нём нет,
            // того не будет и в ответе. Подсказка экономит один неудачный прогон.
            let prompt = if format == OutputFormat::Markdown {
                "Колонки таблицы через пробел (Tab — прошлые)"
            } else {
                "Что должно быть в ответе — поля через пробел (Tab — прошлые)"
            };
            let input: String = step!(Input::with_theme(&theme)
                .with_prompt(prompt)
                .default(current.fields.join(" "))
                .completion_with(&previous_fields)
                .interact_text());
            let fields = format::parse_fields(&input);
            if fields.is_empty() {
                ui::fail("нужно хотя бы одно поле");
                continue;
            }
            break fields;
        }
    };

    // Порядок не случаен: у reasoning-модели бюджет делится с размышлением,
    // и 600 токенов она регулярно тратит, не начав отвечать.
    let limits = [
        "1500 токенов",
        "600 токенов (reasoning-модели часто не хватает)",
        "3000 токенов",
        "Без лимита",
        "Ввести своё значение",
    ];
    let max_tokens = match step!(Select::with_theme(&theme)
        .with_prompt("Ограничение длины")
        .items(limits)
        .default(0)
        .interact())
    {
        0 => Some(1500),
        1 => Some(600),
        2 => Some(3000),
        3 => None,
        _ => Some(step!(Input::with_theme(&theme)
            .with_prompt("Сколько токенов")
            .default(current.max_tokens.unwrap_or(1500))
            .interact_text())),
    };

    let stops = ["Стоп-маркер <END>", "Свой маркер", "Без условия завершения"];
    let stop_marker = match step!(Select::with_theme(&theme)
        .with_prompt("Условие завершения")
        .items(stops)
        .default(0)
        .interact())
    {
        0 => Some("<END>".to_string()),
        1 => Some(step!(Input::with_theme(&theme)
            .with_prompt("Маркер")
            .default(current.stop_marker.clone().unwrap_or_else(|| "<END>".into()))
            .interact_text())),
        _ => None,
    };

    // response_format имеет смысл только когда мы и так просим JSON.
    let json_mode = if format == OutputFormat::Json {
        step!(Confirm::with_theme(&theme)
            .with_prompt("Включить response_format: json_object на стороне API")
            .default(current.json_mode)
            .interact())
    } else {
        false
    };

    // Повторные прогоны доказывают стабильность формата, а диалог требует
    // живых ответов человека и автоматически не повторяется.
    let runs = if dialog {
        1
    } else {
        let counts = [
            "3 прогона",
            "5 прогонов",
            "10 прогонов",
            "20 прогонов (проверка «одна схема — разные данные»)",
            "1 прогон",
            "Ввести своё число",
        ];
        match step!(Select::with_theme(&theme)
            .with_prompt("Сколько раз повторить запрос")
            .items(counts)
            .default(0)
            .interact())
        {
            0 => 3,
            1 => 5,
            2 => 10,
            3 => 20,
            4 => 1,
            _ => step!(Input::<u32>::with_theme(&theme)
                .with_prompt("Сколько прогонов (1..50)")
                .default(current.runs)
                .validate_with(|value: &u32| {
                    if (1..=50).contains(value) {
                        Ok(())
                    } else {
                        Err("от 1 до 50")
                    }
                })
                .interact_text()),
        }
    };

    let constraints = Constraints {
        format,
        fields,
        max_tokens,
        stop_marker,
        json_mode,
        runs,
        dialog,
    };

    ui::header("что уходит в модель");
    println!("{}", summary(&constraints));

    let go = step!(Confirm::with_theme(&theme)
        .with_prompt("Отправить")
        .default(true)
        .interact());
    if !go {
        return Ok(None);
    }

    Ok(Some((question, constraints)))
}

/// Что делать после показанного ответа.
pub fn next_action() -> Result<Next, Box<dyn Error>> {
    let choice = interrupted(
        Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Дальше")
            .items([
                "Повторить с теми же настройками",
                "Изменить настройки",
                "Выход",
            ])
            .default(1)
            .interact(),
    )?;

    Ok(match choice {
        Some(0) => Next::Again,
        Some(1) => Next::Reconfigure,
        _ => Next::Quit,
    })
}

fn summary(c: &Constraints) -> String {
    let mut lines = vec![format!("формат          : {}", c.format.label())];
    if c.format != OutputFormat::Free {
        let label = if c.format == OutputFormat::Markdown {
            "колонки"
        } else {
            "поля"
        };
        lines.push(format!("{label:<16}: {}", c.fields.join(", ")));
    }
    lines.push(format!(
        "max_tokens      : {}",
        c.max_tokens
            .map(|n| n.to_string())
            .unwrap_or_else(|| "не задан".to_string())
    ));
    lines.push(format!(
        "stop            : {}",
        c.stop_marker
            .clone()
            .map(|m| format!("[\"{m}\"]"))
            .unwrap_or_else(|| "не задан".to_string())
    ));
    lines.push(format!(
        "response_format : {}",
        if c.json_mode { "json_object" } else { "не задан" }
    ));
    lines.push(format!(
        "режим           : {}",
        if c.dialog {
            "диалог с уточнениями".to_string()
        } else {
            format!(
                "одиночный запрос, прогонов: {} (+1 запрос без ограничений)",
                c.runs
            )
        }
    ));
    lines.join("\n")
}
