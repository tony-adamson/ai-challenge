//! Индекс документации проекта: чанкинг по разделам (из w5d1), эмбеддинги e5-small, SQLite, косинус.
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

const SIZE: usize = 800;
const OVERLAP: usize = 120;
const SECTION_MAX: usize = 1200;
const SECTION_MIN: usize = 150;
// Сгенерированные отчёты и транскрипты на сотни КБ забивают индекс и время сборки.
const MAX_FILE: u64 = 256 * 1024;

#[derive(Debug)]
pub struct Chunk {
    pub section: String,
    pub text: String,
}

#[derive(Clone)]
pub struct Hit {
    pub score: f32,
    pub source: String,
    pub section: String,
    pub text: String,
}

struct Row {
    source: String,
    section: String,
    text: String,
    emb: Vec<f32>,
}

pub struct Rag {
    model: TextEmbedding,
    rows: Vec<Row>,
    db: PathBuf,
    repo: PathBuf,
    /// Папки верхнего уровня, чей код тоже индексируется (только для `emi review`).
    code_dirs: Vec<String>,
    /// w7d3: папка FAQ поддержки — корпус из её `*.md` вместо tracked-файлов репозитория.
    faq: Option<PathBuf>,
}

impl Rag {
    /// Открывает индекс из `db`; если его нет — собирает.
    pub fn open(repo: &Path, db: &Path) -> Result<Rag, String> {
        let mut rag = Rag { model: model()?, rows: Vec::new(), db: db.into(), repo: repo.into(), code_dirs: Vec::new(), faq: None };
        match load(db) {
            Ok(rows) if !rows.is_empty() => rag.rows = rows,
            _ => {
                eprintln!("индекса нет — собираю {}", db.display());
                eprintln!("{}", rag.reindex()?);
            }
        }
        Ok(rag)
    }

    /// Свежий индекс доков + кода из `code_dirs` в отдельной БД: набор папок свой у каждого PR, старый индекс не годится.
    pub fn build(repo: &Path, db: &Path, code_dirs: Vec<String>) -> Result<Rag, String> {
        let mut rag = Rag { model: model()?, rows: Vec::new(), db: db.into(), repo: repo.into(), code_dirs, faq: None };
        eprintln!("{}", rag.reindex()?);
        Ok(rag)
    }

    /// Индекс FAQ поддержки пересобирается на каждом старте: FAQ маленький, а устаревший индекс не заметить.
    pub fn faq(dir: &Path, db: &Path) -> Result<Rag, String> {
        let mut rag = Rag { model: model()?, rows: Vec::new(), db: db.into(), repo: dir.into(), code_dirs: Vec::new(), faq: Some(dir.into()) };
        eprintln!("{}", rag.reindex()?);
        Ok(rag)
    }

    pub fn reindex(&mut self) -> Result<String, String> {
        let started = Instant::now();
        let files = match &self.faq {
            Some(dir) => faq_corpus(dir)?,
            None => corpus(&self.repo, &self.code_dirs)?,
        };
        let mut chunks = Vec::new(); // (source, chunk)
        for (source, text) in &files {
            for c in chunk_file(source, text) {
                chunks.push((source.clone(), c));
            }
        }
        let mut rows = Vec::with_capacity(chunks.len());
        for batch in chunks.chunks(64) {
            // Путь и раздел — в эмбеддинг: вопрос «как запустить w6d5?» иначе не находит README папки,
            // в тексте которого имя папки не встречается.
            let texts: Vec<String> = batch.iter().map(|(source, c)| format!("passage: {source} · {}\n{}", c.section, c.text)).collect();
            for ((source, c), emb) in batch.iter().zip(embed(&mut self.model, &texts)?) {
                rows.push(Row { source: source.clone(), section: c.section.clone(), text: c.text.clone(), emb });
            }
        }
        save(&self.db, &rows).map_err(|e| format!("не записать индекс {}: {e}", self.db.display()))?;
        self.rows = rows;
        Ok(format!("индекс пересобран: {} файлов, {} фрагментов за {:.1} с", files.len(), self.rows.len(), started.elapsed().as_secs_f32()))
    }

    pub fn search(&mut self, query: &str, k: usize) -> Result<Vec<Hit>, String> {
        let q = embed(&mut self.model, &[format!("query: {query}")])?.remove(0);
        let named = named_dirs(query);
        let mut hits: Vec<Hit> = self
            .rows
            .iter()
            .map(|r| Hit {
                score: r.emb.iter().zip(&q).map(|(a, b)| a * b).sum::<f32>() + path_boost(&r.source, &named),
                source: r.source.clone(),
                section: r.section.clone(),
                text: r.text.clone(),
            })
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(k);
        Ok(hits)
    }
}

// e5-small почти не различает «w6d5» и «w6d4»: папка, названная в вопросе прямо, поднимается над соседями.
const PATH_BOOST: f32 = 0.05;

fn named_dirs(query: &str) -> Vec<String> {
    query.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-')).filter(|w| w.chars().count() >= 3).map(str::to_lowercase).collect()
}

fn path_boost(source: &str, named: &[String]) -> f32 {
    let dirs = source.split('/').rev().skip(1); // имя файла не считаем: «README» в вопросе не должно поднимать все README
    if dirs.map(str::to_lowercase).any(|d| named.contains(&d)) { PATH_BOOST } else { 0.0 }
}

const CODE_EXT: [&str; 5] = [".rs", ".py", ".js", ".ts", ".yml"];

/// ДОПУЩЕНИЕ-1: все tracked `*.md` плюс манифесты как «схема» проекта; код — только из папок `code_dirs`.
fn corpus(repo: &Path, code_dirs: &[String]) -> Result<Vec<(String, String)>, String> {
    let out = Command::new("git")
        .args(["--no-optional-locks", "-C"])
        .arg(repo)
        .args(["ls-files", "-z"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git не запустился: {e}"))?;
    if !out.status.success() {
        return Err(format!("git ls-files: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let mut files = Vec::new();
    for name in String::from_utf8_lossy(&out.stdout).split('\0').filter(|n| !n.is_empty()) {
        let base = name.rsplit('/').next().unwrap_or(name);
        let top = name.split_once('/').map(|(dir, _)| dir);
        // Закоммиченная сборка — не код проекта.
        let code = top.is_some_and(|d| code_dirs.iter().any(|c| c == d)) && CODE_EXT.iter().any(|e| name.ends_with(e)) && !name.split('/').any(|p| p == "target");
        if !(code || name.ends_with(".md") || base == "Cargo.toml" || base == "package.json") {
            continue;
        }
        let path = repo.join(name);
        // Удалённый, но ещё tracked файл или бинарь с .md — пропускаем, индекс важнее одного файла.
        if path.metadata().map_or(true, |m| m.len() > MAX_FILE) {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            files.push((name.to_string(), text));
        }
    }
    if files.is_empty() {
        return Err("в репозитории нет tracked .md, Cargo.toml или package.json — индексировать нечего".into());
    }
    Ok(files)
}

/// `<dir>/*.md` без подпапок, по имени — источник `faq/<имя>` в цитатах.
fn faq_corpus(dir: &Path) -> Result<Vec<(String, String)>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("нет папки FAQ {}: {e}", dir.display()))?;
    let mut names: Vec<String> = entries.filter_map(|e| e.ok()?.file_name().into_string().ok()).filter(|n| n.ends_with(".md") && dir.join(n).is_file()).collect();
    names.sort();
    let mut files = Vec::new();
    for name in names {
        let text = std::fs::read_to_string(dir.join(&name)).map_err(|e| format!("не прочитать {name}: {e}"))?;
        files.push((format!("faq/{name}"), text));
    }
    if files.is_empty() {
        return Err(format!("в {} нет *.md — FAQ пуст", dir.display()));
    }
    Ok(files)
}

/// Markdown режется по разделам; в манифестах `#` — комментарий, а не заголовок, поэтому только окна.
fn chunk_file(source: &str, text: &str) -> Vec<Chunk> {
    if source.ends_with(".md") {
        return chunk(text);
    }
    let chars: Vec<char> = text.chars().collect();
    windows(&chars, SIZE, OVERLAP)
        .into_iter()
        .map(|(s, e)| Chunk { section: String::new(), text: chars[s..e].iter().collect::<String>().trim().to_string() })
        .filter(|c| !c.text.is_empty())
        .collect()
}

/// ДОПУЩЕНИЕ-2: модель общая на все копии дня, в `~/.cache/emi/`.
fn model() -> Result<TextEmbedding, String> {
    let home = std::env::var("HOME").map_err(|_| "не задан HOME — некуда положить кэш модели")?;
    let opts = TextInitOptions::new(EmbeddingModel::MultilingualE5Small)
        .with_cache_dir(PathBuf::from(home).join(".cache/emi"))
        .with_show_download_progress(true);
    TextEmbedding::try_new(opts).map_err(|e| format!("модель эмбеддингов не загрузилась: {e}"))
}

/// e5 vectors, L2-normalized so that cosine = dot product.
fn embed(model: &mut TextEmbedding, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    let mut out = model.embed(texts, None).map_err(|e| format!("эмбеддинги: {e}"))?;
    for v in &mut out {
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
        v.iter_mut().for_each(|x| *x /= norm);
    }
    Ok(out)
}

fn save(db: &Path, rows: &[Row]) -> rusqlite::Result<()> {
    if let Some(dir) = db.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let conn = Connection::open(db)?;
    conn.execute_batch(
        "DROP TABLE IF EXISTS chunks;
         CREATE TABLE chunks (id INTEGER PRIMARY KEY, source TEXT, section TEXT, text TEXT, embedding BLOB);",
    )?;
    let tx = conn.unchecked_transaction()?;
    for r in rows {
        let blob: Vec<u8> = r.emb.iter().flat_map(|x| x.to_le_bytes()).collect();
        tx.execute("INSERT INTO chunks (source, section, text, embedding) VALUES (?1, ?2, ?3, ?4)", params![r.source, r.section, r.text, blob])?;
    }
    tx.commit()
}

fn load(db: &Path) -> rusqlite::Result<Vec<Row>> {
    if !db.exists() {
        return Ok(Vec::new());
    }
    let conn = Connection::open(db)?;
    let mut stmt = conn.prepare("SELECT source, section, text, embedding FROM chunks ORDER BY id")?;
    let rows = stmt.query_map([], |r| {
        let blob: Vec<u8> = r.get(3)?;
        Ok(Row {
            source: r.get(0)?,
            section: r.get(1)?,
            text: r.get(2)?,
            emb: blob.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect(),
        })
    })?;
    rows.collect()
}

// ---------------------------------------------------------------- чанкинг (w5d1, стратегия structure)

/// One chunk per `#`/`##`/`###` section. Sections shorter than SECTION_MIN are glued to the
/// next one unless it climbs to a higher level (then to the previous one); sections longer than
/// SECTION_MAX are re-cut into SIZE windows with OVERLAP, keeping the section path.
pub fn chunk(text: &str) -> Vec<Chunk> {
    chunk_with(text, SECTION_MAX, SIZE, OVERLAP, SECTION_MIN)
}

fn chunk_with(text: &str, max: usize, size: usize, overlap: usize, min: usize) -> Vec<Chunk> {
    let chars: Vec<char> = text.chars().collect();
    let paths = heading_paths(&chars);
    // (start, level, path); level 0 = text before the first heading
    let mut bounds: Vec<(usize, usize, String)> = vec![(0, 0, String::new())];
    bounds.extend(paths.iter().map(|(at, p)| (*at, p.matches(" > ").count() + 1, p.clone())));
    if bounds.len() > 1 && bounds[1].0 == 0 {
        bounds.remove(0);
    }
    // each section keeps the (path, start, end) of every glued part inside its body
    let mut sections: Vec<(Vec<Part>, Vec<char>)> = Vec::new();
    let mut carry: Vec<char> = Vec::new();
    let mut parts: Vec<Part> = Vec::new();
    for (i, (start, level, path)) in bounds.iter().enumerate() {
        let end = bounds.get(i + 1).map_or(chars.len(), |b| b.0);
        parts.push((path.clone(), carry.len(), carry.len() + end - start));
        carry.extend_from_slice(&chars[*start..end]);
        let body_len = carry.iter().collect::<String>().trim().chars().count();
        let next_not_higher = bounds.get(i + 1).is_some_and(|b| b.1 >= *level);
        if body_len < min {
            if next_not_higher {
                continue;
            }
            // nothing to glue forward into (last or climbing section) — glue backward
            if let Some(prev) = sections.last_mut() {
                let shift = prev.1.len();
                prev.0.extend(parts.drain(..).map(|(p, a, b)| (p, a + shift, b + shift)));
                prev.1.append(&mut carry);
                continue;
            }
        }
        sections.push((std::mem::take(&mut parts), std::mem::take(&mut carry)));
    }
    let mut out = Vec::new();
    for (parts, body) in sections {
        let ranges = if body.len() > max { windows(&body, size, overlap) } else { vec![(0, body.len())] };
        for (s, e) in ranges {
            let text = body[s..e].iter().collect::<String>().trim().to_string();
            if !text.is_empty() {
                out.push(Chunk { section: glued_label(&parts, &body, s, e), text });
            }
        }
    }
    out
}

/// (heading path, start, end) of one original section inside a glued body.
type Part = (String, usize, usize);

/// Label of window `s..e`: every part whose non-blank text falls into it, "A > X + Y";
/// an ancestor of another hit part is dropped (its path is already in the child's).
fn glued_label(parts: &[Part], body: &[char], s: usize, e: usize) -> String {
    let hit: Vec<&str> = parts
        .iter()
        .filter(|(p, a, b)| !p.is_empty() && body[(*a).max(s)..(*b).min(e).max((*a).max(s))].iter().any(|c| !c.is_whitespace()))
        .map(|(p, _, _)| p.as_str())
        .collect();
    let hit: Vec<&str> = hit.iter().copied().filter(|p| !hit.iter().any(|q| q.starts_with(&format!("{p} > ")))).collect();
    let parent = |p: &str| p.rfind(" > ").map_or("", |i| &p[..i]).to_string();
    let Some(first) = hit.first() else { return String::new() };
    let mut label = first.to_string();
    for p in &hit[1..] {
        let short = if parent(p) == parent(first) { p.rsplit(" > ").next().unwrap_or(p) } else { p };
        label.push_str(" + ");
        label.push_str(short);
    }
    label
}

/// Char ranges of at most `size` chars; each cut is moved back to whitespace so no word
/// is split, and the next window starts `overlap` chars earlier, at a word start.
fn windows(chars: &[char], size: usize, overlap: usize) -> Vec<(usize, usize)> {
    let n = chars.len();
    let mut out = Vec::new();
    let mut start = 0;
    while start < n {
        let mut end = (start + size).min(n);
        if end < n {
            if let Some(ws) = (start + size / 2..end).rev().find(|&i| chars[i].is_whitespace()) {
                end = ws;
            }
        }
        out.push((start, end));
        if end >= n {
            break;
        }
        // step back to a word start, but by no more than one extra `overlap`
        let next = end.saturating_sub(overlap).max(start + 1);
        let lower = end.saturating_sub(2 * overlap).max(start + 1);
        start = (lower..=next).rev().find(|&i| chars[i - 1].is_whitespace()).unwrap_or(next);
    }
    out
}

/// (char offset of a heading line, heading path "A > B > C"); `#` inside ``` blocks is ignored.
fn heading_paths(chars: &[char]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut in_code = false;
    let mut at = 0;
    for line in chars.split(|&c| c == '\n') {
        let s: String = line.iter().collect();
        let t = s.trim_start();
        if t.starts_with("```") {
            in_code = !in_code;
        } else if !in_code {
            let level = t.chars().take_while(|&c| c == '#').count();
            if (1..=3).contains(&level) && t[level..].starts_with(' ') {
                stack.retain(|(l, _)| *l < level);
                stack.push((level, t[level..].trim().to_string()));
                let path = stack.iter().map(|(_, h)| h.as_str()).collect::<Vec<_>>().join(" > ");
                out.push((at, path));
            }
        }
        at += line.len() + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_get_heading_paths() {
        let body = "текст раздела, достаточно длинный, чтобы не склеиваться с соседями. ".repeat(3);
        let text = format!("# Док\n{body}\n## Запуск\n{body}\n### Флаги\n{body}\n## Команды\n{body}\n");
        let sections: Vec<String> = chunk(&text).into_iter().map(|c| c.section).collect();
        assert_eq!(sections, ["Док", "Док > Запуск", "Док > Запуск > Флаги", "Док > Команды"]);
    }

    #[test]
    fn hash_inside_code_block_is_not_heading() {
        let text = "# Запуск\n```bash\n# комментарий\ncargo run\n```\nконец\n";
        let chunks = chunk(text);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].section, "Запуск");
    }

    #[test]
    fn short_sections_are_glued_and_labelled() {
        let long = "длинный текст раздела ".repeat(10);
        let chunks = chunk(&format!("# Док\n## A\nкоротко\n## B\n{long}"));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].section, "Док > A + B");
    }

    #[test]
    fn long_section_is_recut_within_size() {
        let text = format!("# Глава\n{}", "слово ".repeat(400)); // 2400 chars
        let chunks = chunk(&text);
        assert!(chunks.len() >= 3);
        assert!(chunks.iter().all(|c| c.section == "Глава" && c.text.chars().count() <= SIZE));
    }

    #[test]
    fn folder_named_in_question_is_boosted() {
        let named = named_dirs("как запустить w6d5?");
        assert_eq!(path_boost("w6d5/README.md", &named), PATH_BOOST);
        assert_eq!(path_boost("w6d4/README.md", &named), 0.0);
        assert_eq!(path_boost("README.md", &named_dirs("что в README?")), 0.0);
    }

    #[test]
    fn corpus_takes_code_only_from_touched_dirs() {
        let dir = std::env::temp_dir().join(format!("emi-corpus-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, text) in [("README.md", "# Правила\n"), ("a/main.rs", "fn a() {}"), ("a/target/gen.rs", "fn gen() {}"), ("b/lib.rs", "fn b() {}"), (".env", "KEY=secret")] {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let git = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(&dir).args(args).stdin(Stdio::null()).output().unwrap().status.success());
        git(&["init", "-q"]);
        git(&["add", "-f", "."]);
        let names: Vec<String> = corpus(&dir, &["a".to_string()]).unwrap().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["README.md", "a/main.rs"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn faq_corpus_takes_only_md_files_of_the_folder() {
        let dir = std::env::temp_dir().join(format!("emi-faq-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, text) in [("plans.md", "# Тарифы\n"), ("devices.md", "# Устройства\n"), ("notes.txt", "x"), ("crm.json", "{}"), ("old/archive.md", "# Старое\n")] {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let names: Vec<String> = faq_corpus(&dir).unwrap().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["faq/devices.md", "faq/plans.md"]);
        let _ = std::fs::remove_dir_all(&dir);
        // FAQ демо в репозитории — тоже корпус, и он не пуст.
        assert!(faq_corpus(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/data/faq"))).unwrap().len() >= 5);
    }

    #[test]
    fn manifest_comments_are_not_headings() {
        let chunks = chunk_file("w1d1/Cargo.toml", "# комментарий\n[package]\nname = \"x\"\n");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].section, "");
    }
}
