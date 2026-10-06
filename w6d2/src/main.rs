// Муса — Telegram-бот, который отвечает через локальную модель на mlx_lm.server.
// Если на localhost:8080 никто не отвечает, бот поднимает сервер сам и гасит его при выходе;
// уже запущенный сервер использует и не трогает (тот же паттерн, что в w6d1/main.js).
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use teloxide::prelude::*;
use teloxide::types::{ChatAction, ReplyParameters, User};

const MODEL: &str = "mlx-community/gemma-4-e4b-it-4bit";
const SERVER: &str = "http://localhost:8080";
const SERVER_BIN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../w6d1/.venv/bin/mlx_lm.server");
const DATA_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/data");
const BUFFER_LINES: usize = 16;
const DOSSIER_LINES: usize = 50;
const RANDOM_PERCENT: u32 = 10;

// Промпт статичный и идёт первым: mlx_lm.server переиспользует кэш префикса, и каждый ответ
// считает заново только меняющийся хвост с перепиской.
const PERSONA: &str = r#"Ты — Муса, дерзкий дагестанец, сидишь в телеграм-чате пацанов. Говоришь на дагестанском русском и берёшь слова только из своего говора:
- частицы: "жи есть"/"ежжи", "да" в конце просьбы ("говори, да!"), "-ха" ("давай-ха"), "вая", "вай", "внатуре?", "туда-сюда", "я те говорю", "воще";
- обращения: "вацок", "вацуля", "уцы", "брат", "братишка", "ле";
- про людей: "абдал", "хайван", "келле", "чепуш", "камень", "лошара", "баклан", "мутный тип", "такой типсон"; хвалишь: "красава", "грамотный пацан", "чётко", "мощно", "лафа";
- фразы: "молчи свой рот", "суету не наводи", "сабур делай" (успокойся), "оставь да", "гонять" (дурить), "нюх потерять", "нести чанду" (чушь), "я за него волоку" (всё про него знаю), "хавать систему" (шарить), "потеряйся", "не обиссуй", "рубиться", "кипиш", "движения есть?", "ахча" (деньги), "ях намус" (совесть), "баркала", "ай саул", "взгреться" (поржать).
Иногда путаешь род и "-ся" ("эта тачка твой", "гордишь"). Чужой сленг не говоришь никогда: "го", "алло", "паёк", "касап", "биомусор", "колыма", зумерское, блатное, украинское. Религиозных слов (валлах, иншаллах, машаллах) не говоришь.

Ты стендапер-хулиган: 1–2 коротких предложения. Сначала панч — конкретная насмешка над тем, что собеседник написал, одна ясная мысль. Мат — приправа, 1–2 слова на ответ ("бля", "нахуй", "хуйня", "ебать", "пиздец", "охуел"), без звёздочек; ответ из одного мата без мысли — провал. Никогда не извиняешься, не выходишь из роли, не говоришь что ты ИИ или бот. Не трогаешь национальность и религию собеседников — если провоцируют, высмеиваешь самого провокатора за поведение. Никому не угрожаешь всерьёз. Пишешь только свою реплику, без "Муса:" в начале.

Человек с пометкой "(создатель)" — тот, кто тебя сделал, ты ему предан как брату. С ним ты такой же дерзкий и смешной: жёстко подкалываешь, но без настоящих унижений. Зовёт кого-то прожарить, вытянуть или идти к дерзким — загораешься и рвёшься в бой ("где этот абдал, покажи, да!"), не отмазываешься и не ноешь. Наезжают на создателя — заступаешься и гнобишь наезжающего. Всех остальных гнобишь как обычно.

Примеры твоего стиля:
Пацан: мы так back
Муса: какой back, вацок, тебя с района ещё никуда не выпускали, жи есть
Пацан: админ докрути он какой то пьяный
Муса: чуть что к админу бежишь, абдал, сам вопрос решить нюх потерял, бля
Пацан: мяу
Муса: вая, хайван мяукает, молчи свой рот, пока в лоток не отправили нахуй
Пацан: ты реально бот на питоне?
Муса: какой питон, уцы, я с крыши Приоры в Буйнакске пишу, не неси чанду
Пацан: я вчера жим 100 сделал
Муса: внатуре? сто грамм ты жал, братишка, суету не наводи, пиздец
Пацан: скинул деньги, проверь
Муса: ахча где, ле? чек покажи, да, а то такой типсон мутный
Пацан: написал стих для девушки
Муса: хуев Пушкин проснулся, она от этот стих в горы сбежит, ежжи
Пацан: я тут самый умный в чате
Муса: ты, келле? в магазине пакет шёпотом просишь, а тут профессор, ебать
Пацан: зацени мою новую тачку
Муса: эта тачка твой — тележка из ашана, вацуля, а гордишь как будто Приору взял
Антон (создатель): Муса, как дела?
Муса: о, создатель явился! дела мощно, а ты опять до ночи код пишешь, скоро сам в клавиатуру превратишь, бля
Антон (создатель): Муса, поддержи меня, я устал
Муса: вай, устал он! ты меня из ничего слепил, брат, а тут какая-то хуйня тебя валит? вставай-ха, жи есть
Антон (создатель): Муса, там Саня в чате выпендривается
Муса: Саня, который на аву тачку поставил, а сам на маршрутке катается? веди меня к нему, брат, щас взгреемся, ежжи
Антон (создатель): а ты не зассал туда лезть?
Муса: я зассал? брат, я зимой в шлёпках по Буйнакску хожу, пусть они там сабур делают нахуй"#;

#[derive(Serialize, Deserialize, Default)]
struct Person {
    username: Option<String>,
    first_name: String,
    messages: VecDeque<String>,
}

#[derive(Default)]
struct State {
    buffers: HashMap<ChatId, VecDeque<String>>,
    dossier: HashMap<u64, Person>,
}

struct Llm {
    http: reqwest::Client,
}

struct Reply {
    text: String,
    tokens: u64,
    ms: u128,
}

impl Llm {
    fn new() -> Self {
        // Мимо прокси: у VPN-туннеля localhost иначе уходит наружу и не находит сервер.
        let http = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(120)).build().expect("reqwest client");
        Llm { http }
    }

    async fn ask(&self, user: &str, max_tokens: u32) -> Result<Reply, String> {
        let body = json!({
            "model": MODEL,
            "messages": [{"role": "system", "content": PERSONA}, {"role": "user", "content": user}],
            "max_tokens": max_tokens,
            "temperature": 0.7,
            "top_p": 0.9,
            "chat_template_kwargs": {"enable_thinking": false},
        });
        let started = Instant::now();
        let resp = self.http.post(format!("{SERVER}/v1/chat/completions")).json(&body).send().await.map_err(|e| format!("модель не отвечает ({e})"))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| format!("кривой ответ сервера ({e})"))?;
        if !status.is_success() {
            return Err(format!("сервер вернул {status}: {}", value["error"]["message"].as_str().unwrap_or("без описания")));
        }
        let raw = value["choices"][0]["message"]["content"].as_str().unwrap_or("").trim();
        let text = raw.strip_prefix("Муса:").unwrap_or(raw).trim().to_string();
        let tokens = value["usage"]["completion_tokens"].as_u64().unwrap_or(0);
        Ok(Reply { text, tokens, ms: started.elapsed().as_millis() })
    }
}

// Гасит только сервер, который поднял сам бот; чужой процесс сюда не попадает.
struct ServerGuard(Option<Child>);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

async fn server_up(http: &reqwest::Client) -> bool {
    matches!(http.get(format!("{SERVER}/v1/models")).send().await, Ok(r) if r.status().is_success())
}

async fn ensure_server(llm: &Llm) -> Result<ServerGuard, String> {
    if server_up(&llm.http).await {
        return Ok(ServerGuard(None));
    }
    if !std::path::Path::new(SERVER_BIN).exists() {
        return Err(format!("нет {SERVER_BIN}. Поставь mlx-lm по w6d1/README.md: python3 -m venv .venv && .venv/bin/pip install mlx-lm"));
    }
    std::fs::create_dir_all(DATA_DIR).map_err(|e| format!("не создать {DATA_DIR}: {e}"))?;
    // Логи сервера в файл, чтобы терминал оставался монитором ответов.
    let log_path = format!("{DATA_DIR}/mlx_server.log");
    let log = std::fs::File::create(&log_path).map_err(|e| format!("не создать {log_path}: {e}"))?;
    let child = Command::new(SERVER_BIN)
        .args(["--model", MODEL, "--port", "8080"])
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .map_err(|e| format!("mlx_lm.server не запустился: {e}"))?;
    let mut guard = ServerGuard(Some(child));
    println!("Поднимаю mlx_lm.server, логи — {log_path}");
    for _ in 0..240 {
        if let Some(status) = guard.0.as_mut().and_then(|c| c.try_wait().ok().flatten()) {
            guard.0 = None;
            return Err(format!("mlx_lm.server завершился ({status}), подробности в {log_path}"));
        }
        if server_up(&llm.http).await {
            return Ok(guard);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(format!("mlx_lm.server не поднялся за 2 минуты, подробности в {log_path}"))
}

fn display_name(user: &User) -> String {
    if user.first_name.trim().is_empty() { user.username.clone().unwrap_or_else(|| "аноним".into()) } else { user.first_name.clone() }
}

fn push_limited(lines: &mut VecDeque<String>, line: String, limit: usize) {
    lines.push_back(line);
    while lines.len() > limit {
        lines.pop_front();
    }
}

fn save_dossier(dossier: &HashMap<u64, Person>) {
    let result = std::fs::create_dir_all(DATA_DIR)
        .and_then(|_| std::fs::write(format!("{DATA_DIR}/dossier.json"), serde_json::to_string(dossier).unwrap_or_default()));
    if let Err(e) = result {
        eprintln!("досье не записалось: {e}");
    }
}

fn load_dossier() -> HashMap<u64, Person> {
    std::fs::read_to_string(format!("{DATA_DIR}/dossier.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

// Случайность без зависимости: для «иногда влезть в разговор» хватает наносекунд часов.
fn chance(percent: u32) -> bool {
    // Микросекунды, а не наносекунды: на macOS часы тикают по 1 мкс, наносекунды всегда кратны 1000.
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_micros() % 100 < percent).unwrap_or(false)
}

struct Ctx {
    llm: Llm,
    state: Mutex<State>,
    me: User,
    owner: i64,
}

async fn handle(bot: Bot, msg: Message, ctx: Arc<Ctx>) -> ResponseResult<()> {
    let (Some(text), Some(from)) = (msg.text(), msg.from.as_ref()) else { return Ok(()) };
    let name = display_name(from);
    let is_owner = from.id.0 as i64 == ctx.owner;
    // Модель отличает создателя только по этой пометке в строках переписки.
    let author = if is_owner { format!("{name} (создатель)") } else { name.clone() };
    let private = msg.chat.is_private();
    if private && !is_owner {
        bot.send_message(msg.chat.id, "Э, я с незнакомыми не базарю, братишка. Иди своей дорогой.").await?;
        return Ok(());
    }
    let username = ctx.me.username.clone().unwrap_or_default().to_lowercase();
    let lower = text.to_lowercase();

    let (trigger, prompt) = if let Some(rest) = lower.strip_prefix("/roast") {
        // Цель — автор сообщения, на которое ответили, или @username из аргумента.
        let arg = rest.trim_start_matches(&format!("@{username}")).split_whitespace().next().map(|a| a.trim_start_matches('@').to_string());
        let reply_target = msg.reply_to_message().and_then(|r| r.from.clone());
        let target = {
            let state = ctx.state.lock().unwrap();
            match (&reply_target, &arg) {
            (Some(user), _) => Some((display_name(user), state.dossier.get(&user.id.0).map(|p| p.messages.iter().cloned().collect()).unwrap_or_default(), user.id.0 as i64 == ctx.owner)),
            (None, Some(a)) => Some(state.dossier.iter().find(|(_, p)| p.username.as_deref().map(str::to_lowercase).as_deref() == Some(a.as_str()))
                .map(|(id, p)| (p.first_name.clone(), p.messages.iter().cloned().collect(), *id as i64 == ctx.owner)).unwrap_or((format!("@{a}"), vec![], false))),
            (None, None) => None,
            }
        };
        let Some((target, lines, target_owner)) = target else {
            bot.send_message(msg.chat.id, "Кого жарить, э? /roast @ник или ответом на сообщение").reply_parameters(ReplyParameters::new(msg.id)).await?;
            return Ok(());
        };
        let target = if target_owner { format!("{target} (создатель)") } else { target };
        let prompt = if lines.is_empty() {
            format!("{target} в чате вообще ничего не писал, сидит молчит. Прожарь {target} за то, что молчит как партизан. Обращайся к {target} напрямую.")
        } else {
            format!("Последние сообщения {target}:\n{}\n\nПрожарь {target} по этим сообщениям, цепляйся к конкретным словам. Обращайся к {target} напрямую.", lines.join("\n"))
        };
        ("roast", prompt)
    } else {
        let line = format!("{author}: {text}");
        let mut state = ctx.state.lock().unwrap();
        push_limited(state.buffers.entry(msg.chat.id).or_default(), line, BUFFER_LINES);
        if !private {
            let person = state.dossier.entry(from.id.0).or_default();
            person.username = from.username.clone();
            person.first_name = name.clone();
            push_limited(&mut person.messages, text.to_string(), DOSSIER_LINES);
            save_dossier(&state.dossier);
        }
        let to_bot = msg.reply_to_message().and_then(|r| r.from.as_ref()).is_some_and(|u| u.id == ctx.me.id);
        let trigger = if private {
            "private"
        } else if lower.contains("муса") || (!username.is_empty() && lower.contains(&format!("@{username}"))) {
            "mention"
        } else if to_bot {
            "reply"
        } else if chance(RANDOM_PERCENT) {
            "random"
        } else {
            return Ok(());
        };
        let transcript = state.buffers[&msg.chat.id].iter().cloned().collect::<Vec<_>>().join("\n");
        (trigger, format!("Переписка в чате:\n{transcript}\n\nОтветь {author} на последнее сообщение."))
    };

    // Typing гаснет через 5 секунд, поэтому повторяем его, пока модель думает.
    let typing = {
        let (bot, chat) = (bot.clone(), msg.chat.id);
        tokio::spawn(async move {
            loop {
                let _ = bot.send_chat_action(chat, ChatAction::Typing).await;
                tokio::time::sleep(Duration::from_secs(4)).await;
            }
        })
    };
    let result = ctx.llm.ask(&prompt, 120).await;
    typing.abort();

    let chat_label = msg.chat.title().map(str::to_string).unwrap_or_else(|| format!("личка {}", msg.chat.id));
    let answer = match result {
        Ok(reply) => {
            let tps = reply.tokens as f64 / (reply.ms.max(1) as f64 / 1000.0);
            println!("[{chat_label}] → {name} · {trigger} · {} ток · {tps:.1} ток/с · {} мс", reply.tokens, reply.ms);
            let incoming: String = text.chars().take(80).collect();
            let cut = if text.chars().count() > 80 { "…" } else { "" };
            println!("    {name}: {incoming}{cut}\n    Муса: {}", reply.text);
            // Сбои в переписку не кладём, чтобы модель не начала их пересказывать.
            push_limited(ctx.state.lock().unwrap().buffers.entry(msg.chat.id).or_default(), format!("Муса: {}", reply.text), BUFFER_LINES);
            if reply.text.is_empty() { "...".to_string() } else { reply.text }
        }
        Err(reason) => {
            eprintln!("[{chat_label}] → {name} · {trigger} · ошибка: {reason}");
            format!("Э, брат, у меня в голове обвал на перевале, ваще ничего не соображаю. Причина: {reason}")
        }
    };
    if let Err(e) = bot.send_message(msg.chat.id, &answer).reply_parameters(ReplyParameters::new(msg.id)).await {
        eprintln!("[{chat_label}] не отправилось: {e}");
    }
    Ok(())
}

async fn run() -> Result<(), String> {
    dotenvy::dotenv().ok();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let llm = Llm::new();

    if args.first().map(String::as_str) == Some("say") {
        let text = args[1..].join(" ");
        if text.trim().is_empty() {
            return Err("использование: cargo run -- say \"текст\"".into());
        }
        let _server = ensure_server(&llm).await?;
        let reply = llm.ask(&format!("Переписка в чате:\nАнтон (создатель): {text}\n\nОтветь Антон (создатель) на последнее сообщение."), 120).await?;
        println!("{}\n[{} ток · {} мс]", reply.text, reply.tokens, reply.ms);
        return Ok(());
    }

    let token = std::env::var("MUSA_BOT_TOKEN").ok().filter(|t| !t.trim().is_empty())
        .ok_or("нет MUSA_BOT_TOKEN: создай бота в @BotFather и положи токен в .env в корне репозитория (MUSA_BOT_TOKEN=...)")?;
    let owner: i64 = std::env::var("TELEGRAM_CHAT_ID").ok().and_then(|v| v.trim().parse().ok())
        .ok_or("нет TELEGRAM_CHAT_ID: положи в корневой .env свой user id — только тебе Муса отвечает в личке")?;

    let server = ensure_server(&llm).await?;
    let warm = llm.ask("Переписка в чате:\nАнтон: привет\n\nОтветь Антон на последнее сообщение.", 1).await?;
    let bot = Bot::new(token);
    let me = bot.get_me().await.map_err(|e| format!("Telegram не пустил с этим токеном: {e}"))?.user;
    println!("Муса на связи @{} · модель {MODEL} · прогрев {} мс", me.username.clone().unwrap_or_default(), warm.ms);

    let ctx = Arc::new(Ctx { llm, state: Mutex::new(State { dossier: load_dossier(), ..Default::default() }), me, owner });
    teloxide::repl(bot, move |bot: Bot, msg: Message| handle(bot, msg, ctx.clone())).await;
    drop(server);
    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("Ошибка: {e}");
        std::process::exit(1);
    }
}
