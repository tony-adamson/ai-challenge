//! Оформление вывода: цвет, крутилка, рамки.
//!
//! Цвет включается только для живого терминала — в пайпе и в логе
//! escape-последовательности были бы мусором.

use std::error::Error;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub const RESET: &str = "\x1b[0m";
pub const DIM: &str = "\x1b[2m";
pub const RED: &str = "\x1b[31m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const CYAN: &str = "\x1b[36m";
pub const BOLD: &str = "\x1b[1m";

fn colors_enabled() -> bool {
    std::io::stdout().is_terminal()
}

pub fn paint(text: &str, color: &str) -> String {
    if colors_enabled() {
        format!("{color}{text}{RESET}")
    } else {
        text.to_string()
    }
}

/// Ширина терминала для разделителей; в пайпе берём разумное значение.
fn width() -> usize {
    console::Term::stdout()
        .size_checked()
        .map(|(_, cols)| cols as usize)
        .unwrap_or(80)
        .clamp(40, 100)
}

/// Разделитель секции во всю ширину: глазу нужна линия, а не два прочерка.
pub fn header(title: &str) {
    let text = format!("── {title} ");
    let tail = width().saturating_sub(text.chars().count());
    println!("\n{}", paint(&format!("{text}{}", "─".repeat(tail)), BOLD));
    println!();
}

/// Ответ модели — чужой текст, и он должен визуально отличаться от вывода
/// программы, иначе всё слипается в одну простыню.
pub fn quote(text: &str) {
    for line in text.lines() {
        println!("{} {line}", paint("│", DIM));
    }
}

/// Экран чистится перед новым кругом настройки: иначе поверх ответа модели
/// ложатся строки мастера и читать невозможно.
pub fn clear_screen() {
    let _ = console::Term::stdout().clear_screen();
}

pub fn ok(text: &str) {
    println!("{} {text}", paint("✓", GREEN));
}

pub fn warn(text: &str) {
    println!("{} {text}", paint("✗", YELLOW));
}

pub fn fail(text: &str) {
    println!("{} {text}", paint("✗", RED));
}

/// Пустой ответ — не загадка, а следствие рычага длины: у reasoning-моделей
/// max_tokens тратится и на рассуждение, поэтому маленький лимит съедает
/// ответ целиком, ещё до первого символа.
pub fn explain_empty(finish_reason: &str, reasoning_tokens: u64, max_tokens: Option<u32>) {
    if finish_reason == "length" {
        fail(&format!(
            "лимит в {} токенов израсходован до начала ответа ({reasoning_tokens} ушло на рассуждение) — выбери лимит побольше",
            max_tokens
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".to_string())
        ));
    } else {
        fail(&format!("модель вернула пустой ответ (finish_reason: {finish_reason})"));
    }
}

/// Шаг инициализации: видно, что программа что-то делает до первого вопроса.
pub fn step(text: &str) {
    println!("  {} {text}", paint("✓", GREEN));
}

pub fn note(text: &str) {
    println!("{}", paint(text, DIM));
}

/// Останавливает крутилку в Drop, чтобы паника внутри вызова не оставила
/// поток дописывать анимацию поверх сообщения об ошибке.
struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Крутилка на время блокирующего запроса. Пишет в stderr, чтобы stdout
/// оставался чистым выводом программы.
pub fn with_spinner<T>(
    call: impl FnOnce() -> Result<T, Box<dyn Error>>,
) -> Result<T, Box<dyn Error>> {
    let _spinner = std::io::stderr().is_terminal().then(|| {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
                let mut stderr = std::io::stderr();
                for frame in frames.iter().cycle() {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let _ = write!(stderr, "\r{frame} жду ответа...");
                    let _ = stderr.flush();
                    thread::sleep(Duration::from_millis(80));
                }
                let _ = write!(stderr, "\r\x1b[2K");
                let _ = stderr.flush();
            })
        };
        Spinner {
            stop,
            handle: Some(handle),
        }
    });

    call()
}
