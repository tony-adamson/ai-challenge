//! Уровни контроля ответа: из чего собирается системный промпт и как
//! проверяется то, что вернула модель.
//!
//! Ключевая идея дня: фиксируется структура ответа, а не его содержание.
//! Поэтому проверка сравнивает отпечатки структуры, а значения игнорирует.

use serde_json::Value;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Никаких инструкций о формате — базовая линия для сравнения.
    Free,
    Json,
    Markdown,
}

impl OutputFormat {
    pub fn label(self) -> &'static str {
        match self {
            OutputFormat::Free => "free (без инструкций о формате)",
            OutputFormat::Json => "json",
            OutputFormat::Markdown => "markdown-таблица",
        }
    }
}

/// Все рычаги контроля в одном месте: меню правит эту структуру,
/// а запрос к модели собирается только из неё.
pub struct Constraints {
    pub format: OutputFormat,
    /// Имена полей верхнего уровня — весь контракт формата.
    /// Порядок пользовательский: он же уходит в промпт и в колонки таблицы.
    pub fields: Vec<String>,
    pub max_tokens: Option<u32>,
    pub stop_marker: Option<String>,
    /// response_format: {"type":"json_object"} — рычаг на стороне API,
    /// отдельный от инструкций в промпте.
    pub json_mode: bool,
    pub runs: u32,
    pub dialog: bool,
}

/// Ассистент универсальный, поэтому дефолты не про конкретную тему:
/// запрос пользователь вводит сам, поля — нейтральный каркас ответа.
pub const DEFAULT_QUESTION: &str = "";

pub const DEFAULT_FIELDS: &str = "title summary tags";

/// Поля вводятся строкой через пробел или запятую — разбираем терпимо.
pub fn parse_fields(input: &str) -> Vec<String> {
    input
        .split([' ', ',', '\t', '\n'])
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .map(str::to_string)
        .collect()
}

impl Default for Constraints {
    fn default() -> Self {
        Constraints {
            format: OutputFormat::Json,
            fields: parse_fields(DEFAULT_FIELDS),
            max_tokens: Some(1500),
            stop_marker: Some("<END>".to_string()),
            json_mode: false,
            runs: 3,
            dialog: false,
        }
    }
}

/// Системный промпт для одношагового запроса. `None` — режим без ограничений,
/// тогда модель получает только вопрос пользователя.
pub fn build_system_prompt(c: &Constraints) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    match c.format {
        OutputFormat::Free => {}
        OutputFormat::Json => parts.push(format!(
            "Ты возвращаешь только данные, без разговора с пользователем.\n\
             Ответ — ровно один JSON-объект с полями: {}.\n\
             Ровно эти поля на верхнем уровне: ни одного лишнего, ни одного пропущенного, \
             в этом же порядке.\n\
             Значения — конкретный ответ на запрос, а не описание категории: \
             если у запроса есть решение, назови его прямо.\n\
             Не оборачивай ответ в markdown-блок, не добавляй пояснений до или после JSON.",
            c.fields.join(", ")
        )),
        OutputFormat::Markdown => {
            let columns = c.fields.join(" | ");
            parts.push(format!(
                "Ответ — ровно одна markdown-таблица с колонками: {columns}.\n\
                 Первая строка — заголовок ровно с этими именами и ровно в том порядке, \
                 в каком они перечислены выше; вторая — разделитель вида |---|, \
                 дальше строки данных.\n\
                 Никакого текста до или после таблицы."
            ));
        }
    }

    if c.format != OutputFormat::Free {
        // Без этой строки модель охотно переходит на английский: имена полей
        // в образце английские, и она принимает их за язык ответа.
        parts.push("Значения полей пиши на языке запроса пользователя.".to_string());
    }

    if let Some(limit) = c.max_tokens {
        parts.push(format!(
            "Уложись примерно в {limit} токенов: пиши коротко, без вводных фраз и вежливых оборотов."
        ));
    }

    if let Some(marker) = &c.stop_marker {
        parts.push(format!(
            "Закончив ответ, напиши {marker} и сразу прекрати генерацию. \
             После {marker} не должно быть ни одного символа."
        ));
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// Системный промпт диалогового режима.
///
/// Условие завершения здесь логическое: модель сама решает, хватает ли данных,
/// и сообщает об этом полем `status`. Цикл диалога читает именно его.
pub fn build_dialog_prompt(c: &Constraints) -> String {
    let mut prompt = format!(
        "Ты отвечаешь на запрос пользователя своими знаниями.\n\
         Каждый твой ответ — ровно один JSON-объект, без markdown-обёртки и пояснений.\n\
         Если для ответа не хватает того, что знает только пользователь — его \
         предпочтений, ограничений, контекста задачи:\n\
         {{\"status\": \"asking\", \"question\": \"<один короткий вопрос>\", \"result\": null}}\n\
         Как только можешь ответить:\n\
         {{\"status\": \"done\", \"question\": null, \"result\": {{...}}}}\n\
         В result — ровно поля: {}. Ни одного лишнего, ни одного пропущенного.\n\
         \n\
         Главное правило: НЕ спрашивай то, что можешь знать сам. Свойства предметов, \
         определения, состав, характеристики, факты — заполняй своими знаниями, \
         а не вопросами пользователю. Вопрос уместен только там, где ответ зависит \
         от самого пользователя: его вкусы, бюджет, цель, доступные ему условия.\n\
         Если запроса уже достаточно, чтобы ответить — сразу возвращай \"done\", \
         не задавая ни одного вопроса.\n\
         Значения в result — конкретный ответ с учётом всего, что сказал пользователь, \
         а не общее описание категории: если он просил что-то выбрать или подобрать, \
         назови конкретный вариант.\n\
         Больше трёх вопросов за диалог не задавай, по одному за раз.",
        c.fields.join(", ")
    );

    if let Some(limit) = c.max_tokens {
        prompt.push_str(&format!("\nУложись примерно в {limit} токенов."));
    }
    if let Some(marker) = &c.stop_marker {
        prompt.push_str(&format!(
            "\nПосле закрывающей скобки JSON напиши {marker} и прекрати генерацию."
        ));
    }
    prompt
}

/// Чистит ответ модели: обрезает всё после стоп-маркера и снимает ```-обёртку,
/// если модель её всё-таки добавила.
pub fn extract_payload(raw: &str, stop_marker: Option<&str>) -> String {
    let mut text = raw.trim().to_string();

    if let Some(marker) = stop_marker {
        if let Some(pos) = text.find(marker) {
            text.truncate(pos);
        }
    }

    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("```") {
        // После открывающей ограды идёт язык: либо до перевода строки
        // (```json\n{...}), либо до пробела, если модель уложила всё в одну
        // строку (```json {...}```).
        let body = match rest.split_once('\n') {
            Some((_language, tail)) => tail,
            None => rest.split_once(' ').map(|(_, tail)| tail).unwrap_or(rest),
        };
        // Режем по закрывающей ограде, а не по концу строки: за ней модель
        // могла ещё что-то дописать.
        let body = match body.find("```") {
            Some(end) => &body[..end],
            None => body,
        };
        return body.trim().to_string();
    }

    trimmed.to_string()
}

/// Отпечаток структуры: отсортированный список путей до листьев с их типами.
/// Значения не участвуют — «Япония» и «Франция» дают один отпечаток,
/// а потерянное поле или сменившийся тип дают разный.
pub fn structure_fingerprint(value: &Value) -> String {
    let mut leaves = Vec::new();
    walk(value, "", &mut leaves);
    leaves.sort();
    leaves.dedup();
    leaves.join("\n")
}

fn walk(value: &Value, path: &str, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if map.is_empty() {
                out.push(format!("{path}:empty_object"));
            }
            for (key, child) in map {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                walk(child, &child_path, out);
            }
        }
        Value::Array(items) => {
            if items.is_empty() {
                out.push(format!("{path}[]:empty_array"));
                return;
            }
            // Длина массива — содержание, а вот однородность элементов — структура.
            // Поэтому берём набор различных форм элементов: два ингредиента одной
            // формы дадут один вариант, а потерянное поле в одном из них — второй.
            let mut shapes: Vec<String> = items
                .iter()
                .map(|item| structure_fingerprint(item).replace('\n', ", "))
                .collect();
            shapes.sort();
            shapes.dedup();
            out.push(format!("{path}[]:{{{}}}", shapes.join(" ; ")));
        }
        leaf => {
            let name = type_name(leaf);
            out.push(if path.is_empty() {
                name.to_string()
            } else {
                format!("{path}:{name}")
            });
        }
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Расхождения набора полей ответа с заказанным.
/// Пустой результат — ответ уложился в заданный формат.
pub fn compare_to_fields(actual: &Value, fields: &[String]) -> Vec<String> {
    let Some(map) = actual.as_object() else {
        return vec!["ответ — не JSON-объект".to_string()];
    };

    let mut problems = Vec::new();
    for field in fields {
        if !map.contains_key(field) {
            problems.push(format!("нет поля: {field}"));
        }
    }
    for key in map.keys() {
        if !fields.iter().any(|field| field == key) {
            problems.push(format!("лишнее поле: {key}"));
        }
    }
    problems
}

/// Отпечаток markdown-таблицы: имена колонок заголовка.
/// `None` — таблицы в ответе нет или строки разной ширины.
pub fn markdown_fingerprint(text: &str) -> Option<String> {
    // Внешние пайпы в GFM необязательны: "name | capital" — такая же таблица,
    // как "| name | capital |". Требуем только наличие разделителя колонок.
    let rows: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| line.contains('|'))
        .collect();

    let header = rows.first()?;
    let columns: Vec<String> = split_row(header);
    let width = columns.len();

    for row in &rows[1..] {
        if split_row(row).len() != width {
            return None;
        }
    }
    Some(columns.join(" | "))
}

fn split_row(row: &str) -> Vec<String> {
    row.trim_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn same_shape_different_values_have_equal_fingerprints() {
        let japan = json!({"name": "Япония", "population_mln": 124, "languages": ["японский"]});
        let france = json!({"name": "Франция", "population_mln": 68, "languages": ["французский"]});
        assert_eq!(
            structure_fingerprint(&japan),
            structure_fingerprint(&france)
        );
    }

    #[test]
    fn missing_field_changes_fingerprint() {
        let full = json!({"name": "Япония", "capital": "Токио"});
        let partial = json!({"name": "Япония"});
        assert_ne!(structure_fingerprint(&full), structure_fingerprint(&partial));
    }

    #[test]
    fn changed_type_changes_fingerprint() {
        let number = json!({"population_mln": 124});
        let string = json!({"population_mln": "124"});
        assert_ne!(
            structure_fingerprint(&number),
            structure_fingerprint(&string)
        );
    }

    #[test]
    fn broken_element_inside_array_is_caught() {
        let good = json!({"languages": [{"name": "японский", "official": true},
                                        {"name": "рюкюский", "official": false}]});
        let bad = json!({"languages": [{"name": "японский", "official": true},
                                       {"name": "рюкюский"}]});
        assert_ne!(structure_fingerprint(&good), structure_fingerprint(&bad));
    }

    #[test]
    fn array_length_is_content_not_structure() {
        let one = json!({"languages": [{"name": "японский"}]});
        let three = json!({"languages": [{"name": "японский"},
                                         {"name": "айнский"},
                                         {"name": "рюкюский"}]});
        assert_eq!(structure_fingerprint(&one), structure_fingerprint(&three));
    }

    #[test]
    fn field_comparison_reports_both_directions() {
        let fields = parse_fields("name capital");
        let actual = json!({"name": "Франция", "currency": "евро"});
        let problems = compare_to_fields(&actual, &fields);
        assert_eq!(problems.len(), 2);
        assert!(problems.iter().any(|p| p.contains("нет поля: capital")));
        assert!(problems.iter().any(|p| p.contains("лишнее поле: currency")));
    }

    #[test]
    fn values_do_not_affect_field_check() {
        let fields = parse_fields("name capital");
        for actual in [
            json!({"name": "Япония", "capital": "Токио"}),
            json!({"name": "Чад", "capital": null}),
        ] {
            assert!(compare_to_fields(&actual, &fields).is_empty());
        }
    }

    #[test]
    fn fields_parse_from_spaces_and_commas() {
        assert_eq!(
            parse_fields("  name, capital   population_mln "),
            vec!["name", "capital", "population_mln"]
        );
    }

    #[test]
    fn payload_is_cut_at_stop_marker() {
        let raw = "{\"name\": \"Япония\"}<END>\n\nПриятного изучения!";
        assert_eq!(
            extract_payload(raw, Some("<END>")),
            "{\"name\": \"Япония\"}"
        );
    }

    #[test]
    fn markdown_fence_is_stripped() {
        let raw = "```json\n{\"name\": \"Япония\"}\n```";
        assert_eq!(extract_payload(raw, None), "{\"name\": \"Япония\"}");
    }

    #[test]
    fn single_line_fence_keeps_payload() {
        let raw = "```json {\"name\": \"Япония\"}```";
        assert_eq!(extract_payload(raw, None), "{\"name\": \"Япония\"}");
    }

    #[test]
    fn text_after_closing_fence_is_dropped() {
        let raw = "```json\n{\"name\": \"Япония\"}\n```\n\nНадеюсь, помогло!";
        assert_eq!(extract_payload(raw, None), "{\"name\": \"Япония\"}");
    }

    #[test]
    fn table_without_outer_pipes_is_accepted() {
        let bare = "name | capital\n--- | ---\nЯпония | Токио";
        let piped = "| name | capital |\n|---|---|\n| Франция | Париж |";
        assert_eq!(markdown_fingerprint(bare), markdown_fingerprint(piped));
    }

    #[test]
    fn markdown_table_fingerprint_ignores_values() {
        let first = "| name | capital |\n|---|---|\n| Япония | Токио |";
        let second = "| name | capital |\n|---|---|\n| Франция | Париж |\n| Чад | Нджамена |";
        assert_eq!(markdown_fingerprint(first), markdown_fingerprint(second));
    }

    #[test]
    fn ragged_markdown_table_is_rejected() {
        let ragged = "| name | capital |\n|---|---|\n| Япония |";
        assert_eq!(markdown_fingerprint(ragged), None);
    }

    #[test]
    fn free_format_without_limits_has_no_system_prompt() {
        let constraints = Constraints {
            format: OutputFormat::Free,
            max_tokens: None,
            stop_marker: None,
            ..Constraints::default()
        };
        assert!(build_system_prompt(&constraints).is_none());
    }

    #[test]
    fn columns_keep_the_order_the_user_typed() {
        // Порядок полей пользовательский и должен дойти до модели как есть.
        let prompt = build_system_prompt(&Constraints {
            format: OutputFormat::Markdown,
            fields: parse_fields("name capital population_mln"),
            max_tokens: None,
            stop_marker: None,
            ..Constraints::default()
        })
        .expect("markdown-режим всегда даёт системный промпт");
        assert!(prompt.contains("name | capital | population_mln"));
    }
}
