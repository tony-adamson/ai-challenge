import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import type { Source } from './store.js';

export interface RagConfig { python: string; db: string; roots: { path: string; kind: 'harness' | 'course' | 'custom' }[] }
export function defaultRagConfig(): RagConfig {
  return { python: process.env.W5D5_PYTHON ?? '/opt/homebrew/bin/python3', db: process.env.W5D5_BOOKS_DB ?? '/Users/admin/MyProjects/books-rag/db/books.sqlite3', roots: process.env.W5D5_DOCS_ROOTS ? JSON.parse(process.env.W5D5_DOCS_ROOTS) as RagConfig['roots'] : [{ path: '/Users/admin/MyProjects/g-harness', kind: 'harness' }, { path: process.env.W5D5_COURSE_ROOT ?? fileURLToPath(new URL('../../../', import.meta.url)), kind: 'course' }] };
}
export async function retrieve(query: string, config: RagConfig, signal?: AbortSignal): Promise<Source[]> {
  signal?.throwIfAborted();
  const bounded = signal ? AbortSignal.any([signal, AbortSignal.timeout(15000)]) : AbortSignal.timeout(15000);
  return new Promise((resolve, reject) => {
    const child = spawn(config.python, [fileURLToPath(new URL('../../scripts/search_kb.py', import.meta.url)), '--db', config.db, '--query', query, '--roots', JSON.stringify(config.roots)], { shell: false, stdio: ['ignore', 'pipe', 'pipe'] });
    const output: Buffer[] = []; const errors: Buffer[] = []; let size = 0; let errorSize = 0; let failure: Error | undefined;
    const kill = () => { failure ??= new Error('Локальный RAG отменён или превышен лимит времени'); child.kill('SIGKILL'); };
    bounded.addEventListener('abort', kill, { once: true });
    if (bounded.aborted) kill();
    child.stdout.on('data', (bytes: Buffer) => { size += bytes.length; if (size > 1048576) { failure = new Error('Вывод RAG больше 1 MiB'); child.kill('SIGKILL'); } else output.push(bytes); });
    child.stderr.on('data', (bytes: Buffer) => { errorSize += bytes.length; if (errorSize > 1048576) { failure = new Error('Ошибка RAG больше 1 MiB'); child.kill('SIGKILL'); } else errors.push(bytes); });
    child.on('error', error => { failure = error; });
    child.on('close', code => {
      bounded.removeEventListener('abort', kill);
      if (failure) { reject(failure); return; }
      if (code !== 0) { reject(new Error('Локальный RAG: ' + Buffer.concat(errors).toString('utf8').slice(0, 2000))); return; }
      try {
        const result = JSON.parse(Buffer.concat(output).toString('utf8')) as { sources?: Source[] };
        if (!Array.isArray(result.sources) || result.sources.length > 8 || result.sources.some(source => typeof source.id !== 'string' || typeof source.text !== 'string' || typeof source.title !== 'string' || !['book', 'document'].includes(source.type)) || result.sources.reduce((total, source) => total + source.text.length, 0) > 20000) throw new Error('Неверный формат результата RAG');
        resolve(result.sources);
      } catch (error) { reject(error); }
    });
  });
}
