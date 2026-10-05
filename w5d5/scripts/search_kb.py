import argparse
import hashlib
import json
import os
import pathlib
import re
import sqlite3
import sys

DENIED = {'data', 'state', 'logs', 'ledger', 'reports', 'node_modules', 'target', 'dist', 'release', '__pycache__'}


def files(root, kind):
    if any(part.startswith(".") or part in DENIED for part in root.absolute().parts):
        raise ValueError("Скрытая или служебная папка не может быть источником")
    if root.is_symlink():
        raise ValueError('Корень источника не может быть symlink')
    if root.is_file():
        candidates = [root]
        base = root.parent.resolve()
    else:
        if not root.is_dir():
            raise ValueError('Папка источника недоступна: ' + str(root))
        base = root.resolve()
        candidates = []
        for directory, dirs, names in os.walk(root, followlinks=False):
            dirs[:] = [d for d in dirs if not d.startswith('.') and d not in DENIED and not (pathlib.Path(directory) / d).is_symlink()]
            for name in names:
                candidate = pathlib.Path(directory) / name
                relative = candidate.relative_to(root)
                if kind == 'harness' and not (str(relative) in {'README.md', 'SOLUTION.md', 'SUPERVISOR.md', 'PLAN.md', 'CONCLUSIONS.md'} or relative.parts[0] == 'specs'):
                    continue
                if kind == 'course' and not (str(relative) == 'SOLUTION.md' or len(relative.parts) == 2 and (relative.parts[0] in {'docs', 'specs'} or re.fullmatch(r'w[0-9]+d[0-9]+', relative.parts[0]) and name == 'README.md')):
                    continue
                if candidate.suffix != ".md" or name.startswith("."):
                    continue
                candidates.append(candidate)
                if len(candidates) > 500:
                    raise ValueError('Превышен лимит 500 документов')
    for candidate in sorted(candidates):
        if candidate.suffix != '.md' or candidate.name.startswith('.') or candidate.is_symlink():
            continue
        target = candidate.resolve()
        if not target.is_relative_to(base):
            continue
        if target.stat().st_size > 2 * 1024 * 1024:
            raise ValueError('Документ больше 2 MiB: ' + str(target))
        yield target


def ranked(connection, table, words):
    if not words:
        return []
    for operator in [' AND ', ' OR ']:
        query = operator.join('"' + word + '"*' for word in words)
        rows = connection.execute(f'SELECT * FROM {table} WHERE {table} MATCH ? ORDER BY bm25({table}) LIMIT 4', (query,)).fetchall()
        if rows:
            return rows
    return []


def search(db, query, roots):
    words = list(dict.fromkeys(re.findall(r'[^\W_]+', query.lower(), re.UNICODE)))[:20]
    sources = []
    with sqlite3.connect(pathlib.Path(db).resolve().as_uri() + '?mode=ro', uri=True) as books:
        books.row_factory = sqlite3.Row
        for row in ranked(books, 'chunks', words):
            source = {'id': str(row['source']) + '#' + str(row['chunk_no']), 'type': 'book', 'title': row['title'], 'path': row['source'], 'text': row['text'][:2800]}
            if row['page_start'] is not None:
                first, last = str(row['page_start']), str(row['page_end'] or row['page_start'])
                source['pages'] = first if first == last else first + '–' + last
            sources.append(source)
    with sqlite3.connect(':memory:') as docs:
        docs.row_factory = sqlite3.Row
        docs.execute('CREATE VIRTUAL TABLE documents USING fts5(id UNINDEXED,title UNINDEXED,path UNINDEXED,section UNINDEXED,text)')
        count = 0
        for selected in roots:
            if selected.get('kind') not in {'harness', 'course', 'custom'}:
                raise ValueError('Неизвестный тип корня')
            for path in files(pathlib.Path(selected['path']), selected['kind']):
                count += 1
                if count > 500:
                    raise ValueError('Превышен лимит 500 документов')
                text = path.read_text(encoding='utf8')
                starts = sorted(set([0] + [m.start() for m in re.finditer(r'^#{1,6} .+', text, re.MULTILINE)] + [len(text)]))
                for begin, end in zip(starts, starts[1:]):
                    title = text[begin:end].splitlines()[0] if text[begin:end].strip() else path.name
                    for offset in range(begin, end, 1080):
                        stop = min(offset + 1200, end)
                        section = title + ' · строки ' + str(text.count('\n', 0, offset) + 1) + '–' + str(text.count('\n', 0, stop) + 1)
                        identity = 'doc-' + hashlib.sha256((str(path) + ':' + str(offset)).encode()).hexdigest()[:24]
                        docs.execute('INSERT INTO documents VALUES (?,?,?,?,?)', (identity, path.name, str(path), section, text[offset:stop]))
        for row in ranked(docs, 'documents', words):
            sources.append({'id': row['id'], 'type': 'document', 'title': row['title'], 'path': row['path'], 'section': row['section'], 'text': row['text']})
    return {'sources': sources}


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--db', required=True)
    parser.add_argument('--query', required=True)
    parser.add_argument('--roots', required=True)
    args = parser.parse_args()
    try:
        result = json.dumps(search(args.db, args.query, json.loads(args.roots)), ensure_ascii=False)
        if len(result.encode()) > 1024 * 1024:
            raise ValueError('JSON вывода превышает 1 MiB')
        print(result)
    except Exception as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
