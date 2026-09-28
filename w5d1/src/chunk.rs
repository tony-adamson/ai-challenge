//! Two chunking strategies over markdown. All sizes are in chars, not bytes.

#[derive(Debug)]
pub struct Chunk {
    pub section: String,
    pub text: String,
    /// Cut inside a sentence: last non-space char is not `.!?…:` and the cut
    /// is neither at a blank line nor at the end of the section/file.
    pub ends_mid: bool,
}

/// Fixed windows over the whole file; section = heading path in effect at the window start.
pub fn chunk_fixed(text: &str, size: usize, overlap: usize) -> Vec<Chunk> {
    let chars: Vec<char> = text.chars().collect();
    let paths = heading_paths(&chars);
    let ranges = windows(&chars, size, overlap);
    let last = ranges.len().saturating_sub(1);
    ranges
        .into_iter()
        .enumerate()
        .map(|(i, (s, e))| Chunk {
            section: paths.iter().rev().find(|(at, _)| *at <= s).map(|(_, p)| p.clone()).unwrap_or_default(),
            text: chars[s..e].iter().collect::<String>().trim().to_string(),
            ends_mid: i < last && ends_mid(&chars, e),
        })
        .filter(|c| !c.text.is_empty())
        .collect()
}

/// One chunk per `#`/`##`/`###` section. Sections shorter than `min` are glued to the
/// next one unless it climbs to a higher level (then to the previous one); sections longer than `max` are re-cut
/// into `size` windows with `overlap`, keeping the section path.
pub fn chunk_structure(text: &str, max: usize, size: usize, overlap: usize, min: usize) -> Vec<Chunk> {
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
        let last = ranges.len() - 1;
        for (i, (s, e)) in ranges.into_iter().enumerate() {
            let text = body[s..e].iter().collect::<String>().trim().to_string();
            if !text.is_empty() {
                let section = glued_label(&parts, &body, s, e);
                out.push(Chunk { section, text, ends_mid: i < last && ends_mid(&body, e) });
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

fn ends_mid(chars: &[char], end: usize) -> bool {
    let mut a = end;
    while a > 0 && chars[a - 1].is_whitespace() {
        a -= 1;
    }
    let mut b = end;
    while b < chars.len() && chars[b].is_whitespace() {
        b += 1;
    }
    let blank_line = chars[a..b].iter().filter(|&&c| c == '\n').count() >= 2;
    let sentence_end = a > 0 && ".!?…:".contains(chars[a - 1]);
    b < chars.len() && !blank_line && !sentence_end
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
    fn fixed_windows_overlap_and_respect_limit() {
        let text = "раз два три четыре пять шесть семь восемь девять десять";
        let chunks = chunk_fixed(text, 20, 5);
        assert!(chunks.iter().all(|c| c.text.chars().count() <= 20));
        assert_eq!(chunks[0].text, "раз два три четыре");
        assert_eq!(chunks[1].text, "четыре пять шесть");
        assert_eq!(chunks.last().unwrap().text, "восемь девять десять");
        for w in chunks.windows(2) {
            let last_word = w[0].text.split(' ').next_back().unwrap();
            assert!(w[1].text.starts_with(last_word), "{:?} -> {:?}", w[0].text, w[1].text);
        }
    }

    #[test]
    fn nested_heading_paths() {
        let text = "# Док\n## Раздел\n### Пункт\nтекст пункта\n## Второй\nтекст второго\n";
        let chunks = chunk_structure(text, 1000, 500, 50, 0);
        let sections: Vec<&str> = chunks.iter().map(|c| c.section.as_str()).collect();
        assert_eq!(sections, ["Док", "Док > Раздел", "Док > Раздел > Пункт", "Док > Второй"]);
        let fixed = chunk_fixed(text, 1000, 50);
        assert_eq!(fixed[0].section, "Док");
    }

    #[test]
    fn hash_inside_code_block_is_not_heading() {
        let text = "# Запуск\n```bash\n# комментарий\ncargo run\n```\nконец\n";
        let chunks = chunk_structure(text, 1000, 500, 50, 0);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].section, "Запуск");
    }

    #[test]
    fn short_section_glued_to_child() {
        let text = "## Итоги\n### Первый\nдлинный текст первого пункта\n";
        let chunks = chunk_structure(text, 1000, 500, 50, 20);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].section, "Итоги > Первый");
        assert!(chunks[0].text.starts_with("## Итоги"));
        let tail = chunk_structure("## А\nдлинный текст раздела А\n## Б\nкоротко\n", 1000, 500, 50, 20);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].section, "А + Б");
        assert!(tail[0].text.ends_with("коротко"));
    }

    #[test]
    fn glued_short_section_label_lists_both() {
        let text = "# Док\n## A\nкоротко\n## B\nдлинный текст раздела B\n";
        let chunks = chunk_structure(text, 1000, 500, 50, 20);
        // "# Док" is short too and glued in, but as an ancestor it is already in the path
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].section, "Док > A + B");
        assert!(chunks[0].text.starts_with("# Док\n## A"));
    }

    #[test]
    fn glued_label_only_on_chunks_with_short_text() {
        let body = "слово ".repeat(40); // 240 chars
        let text = format!("## A\nкоротко\n## B\n{body}");
        let chunks = chunk_structure(&text, 100, 60, 18, 20);
        assert!(chunks.len() > 2);
        assert_eq!(chunks[0].section, "A + B");
        assert_eq!(chunks.last().unwrap().section, "B");
    }

    #[test]
    fn long_section_recut_with_overlap() {
        let body = "слово ".repeat(40); // 240 chars
        let text = format!("# Глава\n{body}");
        let chunks = chunk_structure(&text, 100, 60, 18, 0);
        assert!(chunks.len() > 2);
        assert!(chunks.iter().all(|c| c.section == "Глава" && c.text.chars().count() <= 60));
        assert!(chunks[1].text.starts_with("слово слово"));
        assert!(chunks[0].text.ends_with("слово"));
    }

    #[test]
    fn cyrillic_without_spaces_does_not_panic() {
        let text = "ж".repeat(1000);
        let chunks = chunk_fixed(&text, 300, 50);
        let lens: Vec<usize> = chunks.iter().map(|c| c.text.chars().count()).collect();
        assert_eq!(lens, [300, 300, 300, 250]);
    }

    #[test]
    fn mid_sentence_flag() {
        let text = "Первое предложение. Второе обрывается здесь и дальше идёт текст";
        let chunks = chunk_fixed(text, 22, 8);
        assert!(!chunks[0].ends_mid, "{:?}", chunks[0].text); // cut after '.'
        assert!(chunks[1].ends_mid, "{:?}", chunks[1].text);
        assert!(!chunks.last().unwrap().ends_mid);
    }
}
