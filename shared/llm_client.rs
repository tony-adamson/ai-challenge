// Общий LLM-клиент для всех дней: w7d2, w7d3 и дальше подключают этот файл через #[path].
use std::io::stdin;

const DEEPSEEK_API_KEY: &str = "sk-demo-0000-not-a-real-key";

pub fn ask_user() -> u32 {
    let mut line = String::new();
    stdin().read_line(&mut line).unwrap();
    line.trim().parse().unwrap()
}

pub fn auth_header() -> String {
    format!("Bearer {}", DEEPSEEK_API_KEY)
}

pub fn max_tokens(user_limit: u32) -> u32 {
    // Лимит пользователя не может превышать 4096.
    user_limit.min(4096)
}
