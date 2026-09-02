//! Генератор задач с известным ответом. Параметры случайные, эталон
//! считает сам Rust — так точность способов измеряется честно, а свежие
//! числа не могли попасть в обучающие данные модели.

use std::time::{SystemTime, UNIX_EPOCH};

pub const KINDS: [(&str, &str); 4] = [
    ("days", "Дни между датами"),
    ("weekday", "День недели по дате"),
    ("letters", "Подсчёт буквы в строке"),
    ("digits", "Сумма цифр произведения"),
];

pub struct Task {
    pub kind: &'static str,
    pub text: String,
    pub expected: String,
}

/// Xorshift: крейт `rand` ради четырёх генераторов не нужен.
pub struct Rng(u64);

impl Rng {
    pub fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        Rng(nanos | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Случайное число в диапазоне [lo, hi] включительно.
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % (hi - lo + 1) as u64) as i64
    }
}

pub fn generate(kind: &str, rng: &mut Rng) -> Task {
    let kind = match KINDS.iter().find(|(id, _)| *id == kind) {
        Some((id, _)) => *id,
        None => KINDS[rng.range(0, KINDS.len() as i64 - 1) as usize].0,
    };
    match kind {
        "days" => {
            let mut a = random_date(rng);
            let mut b = random_date(rng);
            if days_from_civil(a) > days_from_civil(b) {
                std::mem::swap(&mut a, &mut b);
            }
            let days = days_from_civil(b) - days_from_civil(a);
            Task {
                kind,
                text: format!(
                    "Сколько полных дней прошло с {} по {}? Ответ — одно целое число.",
                    format_date(a),
                    format_date(b)
                ),
                expected: days.to_string(),
            }
        }
        "weekday" => {
            let date = random_date(rng);
            // 1970-01-01 (день 0) был четвергом; индекс 0 — понедельник.
            let index = (days_from_civil(date) + 3).rem_euclid(7) as usize;
            Task {
                kind,
                text: format!(
                    "Какой день недели был {}? Ответ — название дня недели по-русски.",
                    format_date(date)
                ),
                expected: WEEKDAYS[index].to_string(),
            }
        }
        "letters" => {
            // Маленький алфавит, чтобы искомая буква встречалась много раз:
            // считать до трёх модель умеет и без рассуждений.
            const ALPHABET: [char; 5] = ['a', 'e', 'k', 'm', 'o'];
            let len = rng.range(28, 40);
            let text: String = (0..len)
                .map(|_| ALPHABET[rng.range(0, 4) as usize])
                .collect();
            let letter = ALPHABET[rng.range(0, 4) as usize];
            let count = text.chars().filter(|&c| c == letter).count();
            Task {
                kind,
                text: format!(
                    "Сколько раз буква «{letter}» встречается в строке «{text}»? Ответ — одно целое число."
                ),
                expected: count.to_string(),
            }
        }
        _ => {
            let a = rng.range(100, 999);
            let b = rng.range(100, 999);
            let sum: u32 = (a * b).to_string().chars().filter_map(|c| c.to_digit(10)).sum();
            Task {
                kind: "digits",
                text: format!("Чему равна сумма цифр числа {a} × {b}? Ответ — одно целое число."),
                expected: sum.to_string(),
            }
        }
    }
}

const WEEKDAYS: [&str; 7] = [
    "понедельник", "вторник", "среда", "четверг", "пятница", "суббота", "воскресенье",
];

const MONTHS: [&str; 12] = [
    "января", "февраля", "марта", "апреля", "мая", "июня",
    "июля", "августа", "сентября", "октября", "ноября", "декабря",
];

#[derive(Clone, Copy)]
struct Date {
    year: i64,
    month: i64,
    day: i64,
}

fn random_date(rng: &mut Rng) -> Date {
    let year = rng.range(1900, 2099);
    let month = rng.range(1, 12);
    let day = rng.range(1, days_in_month(year, month));
    Date { year, month, day }
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if leap { 29 } else { 28 }
        }
    }
}

fn format_date(d: Date) -> String {
    format!("{} {} {} года", d.day, MONTHS[(d.month - 1) as usize], d.year)
}

/// Число дней от 1970-01-01 по григорианскому календарю
/// (алгоритм Говарда Хиннанта, days_from_civil).
fn days_from_civil(d: Date) -> i64 {
    let y = if d.month <= 2 { d.year - 1 } else { d.year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (d.month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d.day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Итог из ответа модели: содержимое последней строки с «ОТВЕТ:», иначе
/// последняя непустая строка.
pub fn extract_answer(content: &str) -> String {
    const MARKER: &str = "ответ:";
    for line in content.lines().rev() {
        let clean = line.replace(['*', '#', '`'], "").to_lowercase();
        if let Some(pos) = clean.rfind(MARKER) {
            return normalize(&clean[pos + MARKER.len()..]);
        }
    }
    normalize(content.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or(""))
}

pub fn normalize(text: &str) -> String {
    let trimmed = text
        .trim()
        .trim_matches(|c: char| "«»\"'`.:;!?() ".contains(c))
        .to_lowercase();
    trimmed.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Числовой эталон сравнивается с первым числом в ответе (модели любят
/// писать «14 235» или «14,235»), текстовый — по вхождению.
pub fn matches(got: &str, expected: &str) -> bool {
    let expected = normalize(expected);
    let got = normalize(got);
    if let Ok(number) = expected.parse::<i64>() {
        return first_int(&got) == Some(number);
    }
    got == expected || got.contains(&expected)
}

fn first_int(text: &str) -> Option<i64> {
    let chars: Vec<char> = text.chars().collect();
    let start = chars.iter().position(|c| c.is_ascii_digit())?;
    let negative = start > 0 && chars[start - 1] == '-';
    let mut digits = String::new();
    let mut i = start;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_digit() {
            digits.push(c);
        } else if (c == ' ' || c == ',' || c == '\u{a0}')
            && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit())
        {
            // разделитель разрядов
        } else {
            break;
        }
        i += 1;
    }
    let value: i64 = digits.parse().ok()?;
    Some(if negative { -value } else { value })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_and_known_dates() {
        assert_eq!(days_from_civil(Date { year: 1970, month: 1, day: 1 }), 0);
        // 2000-01-01 — 10957-й день от эпохи, суббота.
        let millennium = Date { year: 2000, month: 1, day: 1 };
        assert_eq!(days_from_civil(millennium), 10_957);
        assert_eq!(WEEKDAYS[(days_from_civil(millennium) + 3).rem_euclid(7) as usize], "суббота");
        // 2026-09-02 — среда.
        let today = Date { year: 2026, month: 9, day: 2 };
        assert_eq!(WEEKDAYS[(days_from_civil(today) + 3).rem_euclid(7) as usize], "среда");
    }

    #[test]
    fn answer_extraction() {
        assert_eq!(extract_answer("шаг 1\nшаг 2\n**ОТВЕТ: 14 235**"), "14 235");
        assert_eq!(extract_answer("Ответ: Среда."), "среда");
        assert!(matches("14 235", "14235"));
        assert!(matches("ответ: 7 (семь)", "7"));
        assert!(!matches("17", "7"));
        assert!(matches("это была среда", "среда"));
    }
}
