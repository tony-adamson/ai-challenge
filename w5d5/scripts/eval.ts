import assert from 'node:assert/strict';
import { readFile, mkdir, writeFile, realpath } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { join, sep } from 'node:path';
import { setTimeout as pause } from 'node:timers/promises';
import { startServer } from '../src/server.js';
import { newChatService } from '../src/chat.js';
import { parseVerification, type Verification } from '../src/artifacts.js';
import type { Chat, Source } from '../src/store.js';

interface Expect { goal: string; constraints: string[]; terms?: string[] }
interface Scenario { id: string; title: string; goal: string; restartAfter: number; turns: { content: string; expect: Expect }[]; verification: Verification }
type Operation = { operationId: string; status: string; error?: string };
const project = fileURLToPath(new URL('../../', import.meta.url));
// Explicit handoff after a human has compared every answer and generated instruction to its sources.
if (process.argv[2] === '--review') {
  const base = await realpath(join(project, 'data'));
  const target = await realpath(process.argv[3] ?? '');
  assert.ok(target.startsWith(base + sep + 'eval-') && target.endsWith(sep + 'results.json'), 'Нужен отчёт локального eval-run');
  const report = JSON.parse(await readFile(target, 'utf8')) as { results: { status: string }[]; gates: Record<string, boolean>; semanticReview: string; reviewedAt?: string };
  assert.ok(report.results.length === 2 && report.results.every(result => result.status === 'structural_pass'), 'Сначала исправь неуспешные сценарии');
  report.gates.semanticReview = true; report.semanticReview = 'Ответы, найденные фрагменты и итоговые инструкции вручную сопоставлены'; report.reviewedAt = new Date().toISOString();
  await writeFile(target, JSON.stringify(report, null, 2), { mode: 0o600 });
  console.log(JSON.stringify(report.gates)); process.exit(Object.values(report.gates).every(Boolean) ? 0 : 1);
}
try { process.loadEnvFile(fileURLToPath(new URL('../../../.env', import.meta.url))); }
catch (error) { if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error; }
const root = join(project, 'data', `eval-${new Date().toISOString().replace(/[:.]/g, '-')}`);
await mkdir(root, { recursive: true });
let server: Awaited<ReturnType<typeof startServer>> | undefined;
let cookie = '';
const deadline = AbortSignal.timeout(3_600_000);
const shutdown = new AbortController();
const signal = AbortSignal.any([deadline, shutdown.signal]);
process.once('SIGINT', () => shutdown.abort());
process.once('SIGTERM', () => shutdown.abort());
signal.addEventListener('abort', () => { void server?.close(); }, { once: true });
async function api<T>(route: string, payload?: unknown): Promise<T> {
  signal.throwIfAborted();
  const response = await fetch(server!.origin + route, { method: payload === undefined ? 'GET' : 'POST', headers: { Cookie: cookie, 'Content-Type': 'application/json' }, ...(payload === undefined ? {} : { body: JSON.stringify(payload) }), signal: AbortSignal.any([signal, AbortSignal.timeout(10_000)]) });
  const result = await response.json() as T & { error?: string };
  if (!response.ok) throw new Error(result.error ?? `HTTP ${response.status}`);
  return result;
}
async function open(): Promise<void> {
  let service: ReturnType<typeof newChatService>;
  server = await startServer({ root, action: (...args) => service.run(...args) });
  service = newChatService(server.store);
  const response = await fetch(server.origin + '/api/session', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ token: new URL(server.bootstrapUrl).hash.slice(1) }), signal });
  assert.equal(response.status, 200);
  cookie = response.headers.get('set-cookie')!.split(';')[0]!;
}
async function act(id: string, action: string, payload: unknown): Promise<void> {
  let operation = await api<Operation>(`/api/chats/${id}/${action}`, payload);
  while (operation.status === 'running') {
    await pause(400, undefined, { signal });
    operation = await api<Operation>(`/api/chats/${id}/operations/${operation.operationId}`);
  }
  assert.equal(operation.status, 'completed', operation.error ?? action);
}
function memory(chat: Chat, expected: Expect, turn: number): void {
  assert.ok(chat.task.goal?.value.includes(expected.goal), 'Цель потеряна');
  for (const value of expected.constraints) assert.ok(chat.task.constraints.some(entry => entry.value.includes(value)), `Нет ограничения: ${value}`);
  for (const value of expected.terms ?? []) assert.ok(chat.task.terms.some(entry => entry.value.includes(value)), `Нет термина: ${value}`);
  if (turn >= 8) assert.ok(!chat.task.constraints.some(entry => entry.value.includes('связные абзацы')), 'Старое ограничение не заменено');
  for (const entry of [chat.task.goal!, ...chat.task.constraints, ...chat.task.terms, ...chat.task.clarifications]) {
    const user = chat.messages.find(message => message.id === entry.messageId && message.role === 'user');
    assert.ok(user && entry.quote && user.content.includes(entry.quote), 'У памяти нет пользовательского основания');
  }
}
async function turn(id: string, content: string): Promise<Chat> {
  await act(id, 'ask', { content });
  const chat = await api<Chat>(`/api/chats/${id}`);
  const user = chat.messages.filter(message => message.role === 'user').at(-1)!;
  const answer = chat.messages.find(message => message.role === 'assistant' && message.replyTo === user.id && message.status === 'accepted');
  assert.ok(Array.isArray(user.retrieval), 'Не выполнялся локальный RAG');
  assert.ok(answer, 'Нет принятого ответа');
  for (const citation of answer.citations ?? []) {
    const source: Source | undefined = user.retrieval.find(item => item.id === citation.id);
    assert.ok(source && source.text.includes(citation.quote), 'Цитата не из найденного фрагмента');
    const fetched: { text: string } = await api<{ text: string }>(`/api/chats/${id}/sources/${answer.id}/${encodeURIComponent(citation.id)}`);
    assert.equal(fetched.text, source.text);
  }
  return chat;
}
const results: { id: string; chatId?: string; status: string; turns: number; artifactId?: string; error?: string; mcp?: string[] }[] = [];
try {
  await open();
  for (const id of ['docs', 'review']) {
    const scenario = JSON.parse(await readFile(join(project, 'tests', 'scenarios', `${id}.json`), 'utf8')) as Scenario;
    assert.equal(scenario.turns.length, 12); parseVerification(scenario.verification);
    const result: typeof results[number] = { id, status: 'running', turns: 0 }; results.push(result);
    try {
      let chat = await api<Chat>('/api/chats', { settings: { provider: 'cerebras', model: 'qwen-3.8-27b', temperature: 0.3, reasoning: 'none' } });
      result.chatId = chat.id;
      for (const [index, input] of scenario.turns.entries()) {
        chat = await turn(chat.id, input.content); memory(chat, input.expect, index + 1); result.turns++;
        console.log(`${id}: ${result.turns}/13 — память и происхождение источников подтверждены`);
        if (index + 1 === scenario.restartAfter) {
          const before = JSON.stringify(chat); await server!.close(); await open();
          chat = await api<Chat>(`/api/chats/${chat.id}`); assert.equal(JSON.stringify(chat), before, 'История изменена при перезапуске');
        }
      }
      chat = await turn(chat.id, 'Подтверждаю критерии проверки:\n' + JSON.stringify(scenario.verification)); result.turns++;
      memory(chat, scenario.turns.at(-1)!.expect, 13);
      result.mcp = [...new Set(chat.messages.flatMap(message => (message.toolTraces ?? []).filter(trace => !trace.error && Array.isArray(trace.sourceIds) && trace.sourceIds.length).map(trace => String(trace.server))))];
      await act(chat.id, 'build', {}); chat = await api<Chat>(`/api/chats/${chat.id}`);
      const artifact = chat.artifacts.at(-1)!; result.artifactId = artifact.id;
      await act(chat.id, 'validate', { artifactId: artifact.id });
      const report = await api<{ content: string }>(`/api/chats/${chat.id}/artifacts/${artifact.id}/files/validation`);
      const validation = JSON.parse(report.content) as { status: string; cases: { status: string; model?: string }[] };
      assert.equal(validation.status, 'passed'); assert.equal(validation.cases.length, 3);
      assert.ok(validation.cases.every(item => item.status === 'passed' && item.model));
      result.status = 'structural_pass';
    } catch (error) { result.status = 'failed'; result.error = error instanceof Error ? error.message : 'Неизвестная ошибка'; }
    await writeFile(join(root, 'results.json'), JSON.stringify({ results, semanticReview: 'pending' }, null, 2), { mode: 0o600 });
    console.log(`${id}: ${result.status}${result.error ? ` — ${result.error}` : ''}`);
  }
} finally { await server?.close(); }
const mcp = new Set(results.flatMap(result => result.mcp ?? []));
const gates = { scenarios: results.length === 2 && results.every(result => result.status === 'structural_pass'), research: mcp.has('research'), deepwiki: mcp.has('deepwiki'), semanticReview: false };
await writeFile(join(root, 'results.json'), JSON.stringify({ results, gates, semanticReview: 'Ручная сверка источников и инструкций ещё обязательна' }, null, 2), { mode: 0o600 });
console.log(`Отчёт: ${join(root, 'results.json')}`);
console.log(JSON.stringify(gates));
process.exitCode = Object.values(gates).every(Boolean) ? 0 : 1;
