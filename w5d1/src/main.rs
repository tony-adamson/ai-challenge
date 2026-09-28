mod chunk;

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use rusqlite::{params, Connection};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Instant;

type Res<T> = Result<T, Box<dyn Error>>;

const SIZE: usize = 800;
const OVERLAP: usize = 120;
const SECTION_MAX: usize = 1200;
const SECTION_MIN: usize = 150;
const STRATEGIES: [&str; 2] = ["fixed", "structure"];

/// Control questions: (question, expected source, expected section substring).
const QUESTIONS: [(&str, &str, &str); 10] = [
    ("Почему агент без состояния предложит решение на Python, даже если я всегда пишу на Kotlin?", "docs/transcript_lektsii_po_ii.md", "Stateless"),
    ("Чем плохо отправлять профиль, историю и все ограничения в каждом запросе сразу?", "docs/transcript_lektsii_po_ii.md", "Антипаттерны"),
    ("Влияет ли температура на правильность ответа в задачах с единственным верным решением?", "w1d4/README.md", "Что показали прогоны"),
    ("Почему сворачивание истории в пересказ поначалу не экономит, а только добавляет расходов?", "w2d4/README.md", "Что теряется"),
    ("Как работает стратегия, которая отправляет модели только несколько последних сообщений?", "w2d5/README.md", "Скользящее окно"),
    ("Как отдельный вызов модели проверяет готовый ответ на нарушение ограничений до показа?", "w3d4/README.md", "Двойная защита"),
    ("Как часто уходит дайджест по наблюдениям в Telegram и что приходит, если ничего не поменялось?", "w4d3/README.md", "Цикл сводок"),
    ("Почему не стали объединять все MCP-серверы за одним прокси?", "SOLUTION.md", "Вариант 2"),
    ("Где узнать, сколько долларов стоил конкретный вызов модели?", "w1d5/README.md", "Где смотреть стоимость"),
    ("Что станет с задачей на паузе, если перезапустить сервер, а потом продолжить?", "w3d3/README.md", "Пауза и продолжение"),
];

struct Row {
    source: String,
    section: String,
    text: String,
    ends_mid: bool,
    emb: Vec<f32>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("ошибка: {e}");
        std::process::exit(1);
    }
}

fn run() -> Res<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = flag("--root").map(PathBuf::from).unwrap_or_else(|| here.join(".."));
    let db = here.join("index.db");
    match args.first().map(String::as_str) {
        Some("index") => index(&root, &here, &db),
        Some("stats") => {
            let rows = load(&db)?;
            print_stats(&rows);
            Ok(())
        }
        Some("search") => {
            let query = args.get(1).filter(|q| !q.starts_with('-')).ok_or("нужен запрос: search \"<вопрос>\"")?;
            let strategy = flag("--strategy").unwrap_or_else(|| "structure".into());
            if !STRATEGIES.contains(&strategy.as_str()) {
                return Err(format!("неизвестная стратегия {strategy}: fixed или structure").into());
            }
            let k: usize = flag("-k").map_or(Ok(5), |v| v.parse()).map_err(|_| "-k должно быть числом")?;
            let rows = load(&db)?;
            let q = embed(&mut model(&here)?, &[format!("query: {query}")])?.remove(0);
            for (rank, (score, r)) in top(&rows[&strategy as &str], &q, k).into_iter().enumerate() {
                println!("{}. {score:.3}  {} · {}\n   {}\n", rank + 1, r.source, r.section, preview(&r.text, 200));
            }
            Ok(())
        }
        Some("compare") => compare(&here, &db),
        _ => Err("команды: index | stats | search \"<вопрос>\" [--strategy fixed|structure] [-k N] | compare [--root DIR]".into()),
    }
}

fn corpus(root: &Path) -> Res<Vec<(String, String)>> {
    let mut files = vec!["SOLUTION.md".to_string(), "docs/transcript_lektsii_po_ii.md".to_string()];
    let mut more = Vec::new();
    for dir in std::fs::read_dir(root.join("specs"))? {
        let name = dir?.file_name().to_string_lossy().to_string();
        if name.ends_with(".md") {
            more.push(format!("specs/{name}"));
        }
    }
    for dir in std::fs::read_dir(root)? {
        let name = dir?.file_name().to_string_lossy().to_string();
        let day = name.len() == 4 && name.starts_with('w') && name.as_bytes()[2] == b'd';
        if day && name != "w5d1" && root.join(&name).join("README.md").exists() {
            more.push(format!("{name}/README.md"));
        }
    }
    more.sort();
    files.extend(more);
    files
        .into_iter()
        .map(|f| match std::fs::read_to_string(root.join(&f)) {
            Ok(text) => Ok((f, text)),
            Err(e) => Err(format!("не прочитать {f}: {e}").into()),
        })
        .collect()
}

fn index(root: &Path, here: &Path, db: &Path) -> Res<()> {
    let started = Instant::now();
    let files = corpus(root)?;
    let mut all = Vec::new(); // (strategy, source, title, ord, chunk)
    for (source, text) in &files {
        let title = text.lines().find_map(|l| l.strip_prefix("# ")).unwrap_or(source).trim().to_string();
        let fixed = chunk::chunk_fixed(text, SIZE, OVERLAP);
        let structure = chunk::chunk_structure(text, SECTION_MAX, SIZE, OVERLAP, SECTION_MIN);
        for (strategy, chunks) in [("fixed", fixed), ("structure", structure)] {
            for (ord, c) in chunks.into_iter().enumerate() {
                all.push((strategy, source.clone(), title.clone(), ord, c));
            }
        }
    }
    eprintln!("корпус: {} файлов, {} чанков (обе стратегии)", files.len(), all.len());
    let mut model = model(here)?;
    let conn = Connection::open(db)?;
    conn.execute_batch(
        "DROP TABLE IF EXISTS chunks;
         CREATE TABLE chunks (chunk_id TEXT PRIMARY KEY, strategy TEXT, source TEXT, title TEXT,
           section TEXT, ord INTEGER, text TEXT, ends_mid INTEGER, embedding BLOB);",
    )?;
    let tx = conn.unchecked_transaction()?;
    for (n, batch) in all.chunks(64).enumerate() {
        let texts: Vec<String> = batch.iter().map(|(.., c)| format!("passage: {}", c.text)).collect();
        for ((strategy, source, title, ord, c), emb) in batch.iter().zip(embed(&mut model, &texts)?) {
            let blob: Vec<u8> = emb.iter().flat_map(|x| x.to_le_bytes()).collect();
            tx.execute(
                "INSERT INTO chunks VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![format!("{strategy}:{source}:{ord:04}"), strategy, source, title, c.section, *ord as i64, c.text, c.ends_mid, blob],
            )?;
        }
        eprint!("\rэмбеддинги: {}/{}", (n * 64 + batch.len()), all.len());
    }
    tx.commit()?;
    eprintln!("\nиндекс {} готов за {:.1} с", db.display(), started.elapsed().as_secs_f32());
    Ok(())
}

fn model(here: &Path) -> Res<TextEmbedding> {
    let opts = TextInitOptions::new(EmbeddingModel::MultilingualE5Small)
        .with_cache_dir(here.join(".fastembed_cache"))
        .with_show_download_progress(true);
    Ok(TextEmbedding::try_new(opts)?)
}

/// e5 vectors, L2-normalized so that cosine = dot product.
fn embed(model: &mut TextEmbedding, texts: &[String]) -> Res<Vec<Vec<f32>>> {
    let mut out = model.embed(texts, None)?;
    for v in &mut out {
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
        v.iter_mut().for_each(|x| *x /= norm);
    }
    Ok(out)
}

fn load(db: &Path) -> Res<std::collections::BTreeMap<&'static str, Vec<Row>>> {
    let missing = || -> Box<dyn Error> { "индекса нет — сначала запусти `cargo run --release -- index`".into() };
    if !db.exists() {
        return Err(missing());
    }
    let conn = Connection::open(db)?;
    let mut map = std::collections::BTreeMap::new();
    for strategy in STRATEGIES {
        let mut stmt = conn
            .prepare("SELECT source, section, text, ends_mid, embedding FROM chunks WHERE strategy = ?1 ORDER BY chunk_id")
            .map_err(|_| missing())?;
        let rows = stmt.query_map([strategy], |r| {
            let blob: Vec<u8> = r.get(4)?;
            Ok(Row {
                source: r.get(0)?,
                section: r.get(1)?,
                text: r.get(2)?,
                ends_mid: r.get(3)?,
                emb: blob.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect(),
            })
        })?;
        map.insert(strategy, rows.collect::<Result<Vec<_>, _>>()?);
    }
    if map.values().all(Vec::is_empty) {
        return Err(missing());
    }
    Ok(map)
}

fn top<'a>(rows: &'a [Row], q: &[f32], k: usize) -> Vec<(f32, &'a Row)> {
    let mut scored: Vec<(f32, &Row)> = rows.iter().map(|r| (r.emb.iter().zip(q).map(|(a, b)| a * b).sum(), r)).collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.truncate(k);
    scored
}

fn preview(text: &str, n: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut s: String = flat.chars().take(n).collect();
    if flat.chars().count() > n {
        s.push('…');
    }
    s
}

fn print_stats(rows: &std::collections::BTreeMap<&str, Vec<Row>>) {
    println!("{:<10} {:>6} {:>5} {:>5} {:>6} {:>5} {:>9} {:>8}", "стратегия", "чанков", "min", "avg", "median", "max", "обрыв, %", "секций");
    for (strategy, rs) in rows {
        let mut lens: Vec<usize> = rs.iter().map(|r| r.text.chars().count()).collect();
        lens.sort();
        let n = lens.len().max(1);
        let mid = rs.iter().filter(|r| r.ends_mid).count() as f32 * 100.0 / n as f32;
        let sections: std::collections::HashSet<_> = rs.iter().map(|r| (&r.source, &r.section)).collect();
        println!(
            "{:<10} {:>6} {:>5} {:>5} {:>6} {:>5} {:>9.1} {:>8}",
            strategy,
            lens.len(),
            lens.first().unwrap_or(&0),
            lens.iter().sum::<usize>() / n,
            lens.get(lens.len() / 2).unwrap_or(&0),
            lens.last().unwrap_or(&0),
            mid,
            sections.len()
        );
    }
}

fn compare(here: &Path, db: &Path) -> Res<()> {
    let rows = load(db)?;
    print_stats(&rows);
    let mut model = model(here)?;
    let queries: Vec<String> = QUESTIONS.iter().map(|(q, ..)| format!("query: {q}")).collect();
    let qv = embed(&mut model, &queries)?;
    // hits[strategy] = [src@1, src@3, sec@1, sec@3]
    let mut hits = [[0; 4]; 2];
    println!();
    for (i, (q, source, section)) in QUESTIONS.iter().enumerate() {
        println!("{:>2}. {q}\n    ожидается: {source} · {section}", i + 1);
        for (s, strategy) in STRATEGIES.iter().enumerate() {
            let found = top(&rows[strategy], &qv[i], 3);
            let src = |r: &Row| r.source == *source;
            let sec = |r: &Row| src(r) && r.section.contains(section);
            let checks = [src(found[0].1), found.iter().any(|f| src(f.1)), sec(found[0].1), found.iter().any(|f| sec(f.1))];
            for (h, ok) in hits[s].iter_mut().zip(checks) {
                *h += ok as usize;
            }
            let mark = if checks[2] { "✓" } else if checks[0] { "~" } else { "✗" };
            let r = found[0].1;
            println!("    {mark} {:<9} {} · {}\n                {}", strategy, r.source, r.section, preview(&r.text, 80));
        }
        println!();
    }
    let n = QUESTIONS.len();
    println!("{:<10} {:>12} {:>12} {:>14} {:>14}", "стратегия", "source@1", "source@3", "+section@1", "+section@3");
    for (s, strategy) in STRATEGIES.iter().enumerate() {
        let h = hits[s].map(|x| format!("{x}/{n}"));
        println!("{:<10} {:>12} {:>12} {:>14} {:>14}", strategy, h[0], h[1], h[2], h[3]);
    }
    println!("\n✓ source и section совпали · ~ только source · ✗ мимо");
    Ok(())
}
