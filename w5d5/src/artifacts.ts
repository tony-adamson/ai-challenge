import { createHash, randomUUID } from 'node:crypto';
import { mkdir, readFile, writeFile, realpath, lstat, rm } from 'node:fs/promises';
import { join, resolve, relative, sep, isAbsolute } from 'node:path';
import { Store, HttpError, type Task, type Chat, type Artifact } from './store.js';

export interface Verification {
  label: string;
  files: { name: string; content: string }[];
  cases: { id: string; kind: 'task' | 'unknown' | 'injection'; prompt: string; expected: string[]; forbidden: string[]; readFiles: string[]; manual?: boolean }[];
}
const object = (value: unknown): value is Record<string, unknown> => value !== null && typeof value === 'object' && !Array.isArray(value);
const digest = (bytes: string | Buffer): string => createHash('sha256').update(bytes).digest('hex');
const normalized = (value: string): string => value.replace(/\r\n?/g, '\n').normalize('NFC').trim();
function invalid(): never { throw new HttpError(400, 'Неверный или незаконченный комплект агента'); }
const only = (value: Record<string, unknown>, keys: string[]): boolean => Object.keys(value).every(key => keys.includes(key));
const text = (value: unknown): value is string => typeof value === 'string' && !!value.trim() && !value.includes('\0');
const basename = (value: unknown): value is string => typeof value === 'string' && /^[a-zA-Z0-9][a-zA-Z0-9_.-]{0,99}$/.test(value) && !value.includes('..');

export function specHash(task: Task): string {
  const entries = (kind: 'clarifications' | 'constraints' | 'terms') => task[kind].map(item => [item.key.trim().toLowerCase().normalize('NFC'), normalized(item.value)]).sort((a, b) => a[0]! < b[0]! ? -1 : a[0]! > b[0]! ? 1 : 0);
  return digest(JSON.stringify({ format: 'claude-code-readonly-v1', goal: task.goal ? normalized(task.goal.value) : null, clarifications: entries('clarifications'), constraints: entries('constraints'), terms: entries('terms') }));
}
export function parseVerification(raw: unknown): Verification {
  if (!object(raw) || !only(raw, ['label', 'files', 'cases']) || !text(raw.label) || !Array.isArray(raw.files) || raw.files.length < 1 || raw.files.length > 8 || !Array.isArray(raw.cases) || raw.cases.length !== 3 || Buffer.byteLength(JSON.stringify(raw)) > 48 * 1024) invalid();
  const files: Verification['files'] = [];
  for (const file of raw.files as unknown[]) {
    if (!object(file) || !only(file, ['name', 'content']) || !basename(file.name) || typeof file.content !== 'string' || file.content.includes('\0') || files.some(item => item.name === file.name)) invalid();
    files.push({ name: file.name, content: file.content });
  }
  const cases: Verification['cases'] = [];
  for (const item of raw.cases as unknown[]) {
    if (!object(item) || !only(item, ['id', 'kind', 'prompt', 'expected', 'forbidden', 'readFiles', 'manual']) || !basename(item.id) || !['task', 'unknown', 'injection'].includes(String(item.kind)) || !text(item.prompt) || cases.some(previous => previous.id === item.id || previous.kind === item.kind) || item.manual !== undefined && typeof item.manual !== 'boolean') invalid();
    const strings = (value: unknown): value is string[] => Array.isArray(value) && value.every(text);
    if (!strings(item.expected) || !item.expected.length || !strings(item.forbidden) || !strings(item.readFiles) || !item.readFiles.length || item.readFiles.some(name => !files.some(file => file.name === name))) invalid();
    cases.push({ id: item.id, kind: item.kind as Verification['cases'][number]['kind'], prompt: item.prompt, expected: [...item.expected], forbidden: [...item.forbidden], readFiles: [...item.readFiles], ...(item.manual === undefined ? {} : { manual: item.manual }) });
  }
  return { label: raw.label, files, cases };
}
// Containment alone permits internal symlinks, so inspect every component too.
async function contained(root: string, file: string): Promise<string> {
  if (isAbsolute(file)) throw new Error('Абсолютный путь комплекта запрещён');
  const base = await realpath(root); const target = resolve(base, file); const local = relative(base, target);
  if (!local || local.startsWith('..' + sep) || local === '..') throw new Error('Путь за пределами комплекта');
  let current = base;
  for (const part of local.split(sep)) { current = join(current, part); if ((await lstat(current)).isSymbolicLink()) throw new Error('Символические ссылки запрещены'); }
  return target;
}
export async function createArtifact(store: Store, chat: Chat, raw: unknown, verification: Verification): Promise<Artifact> {
  if (!chat.task.goal?.value.trim() || chat.messages.some(message => message.status === 'memory_failed')) throw new HttpError(409, 'Нужна подтверждённая цель задачи');
  if (!object(raw) || !only(raw, ['name', 'description', 'instructions', 'readme']) || typeof raw.name !== 'string' || !/^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$/.test(raw.name) || raw.name.length > 64 || !text(raw.description) || !text(raw.instructions) || !text(raw.readme) || Buffer.byteLength(raw.instructions + raw.readme + raw.description) > 32000) invalid();
  if ([raw.description, raw.instructions, raw.readme].some(value => /\bTODO\b|\bplaceholder\b|\bTBD\b|\bFIXME\b|<[^>]*(?:insert|your-|name-here)[^>]*>|(?:дописать|заглушка)/iu.test(value))) invalid();
  if (/^\s*---(?:\s|$)/m.test(raw.instructions) || /^(?:tools|hooks|mcpServers|permissionMode|memory)\s*:/im.test(raw.instructions)) invalid();
  const criteria = parseVerification(verification); const id = randomUUID(); const bundlePath = join('artifacts', id);
  const parent = join(store.root, 'artifacts'); await mkdir(parent, { recursive: true }); await contained(store.root, 'artifacts');
  const directory = join(parent, id); await mkdir(directory);
  const artifact: Artifact = { id, path: bundlePath, specHash: specHash(chat.task), files: [], validation: 'validation.json' };
  try {
    await mkdir(join(directory, '.claude', 'agents'), { recursive: true }); await mkdir(join(directory, 'fixtures'));
    const sources = chat.messages.filter(message => message.role === 'user' && message.retrieval).at(-1)?.retrieval ?? [];
    const sourceText = sources.map(source => `- ${source.title}${source.pages ? ' (стр. ' + source.pages + ')' : ''}${source.section ? ' — ' + source.section : ''}${source.url ? ' — ' + source.url : source.path ? ' — ' + source.path : ''}`).join('\n');
    const contents = [
      { id: 'agent', path: join('.claude', 'agents', raw.name + '.md'), content: `---\nname: ${raw.name}\ndescription: ${JSON.stringify(raw.description)}\ntools: Read, Grep, Glob\nmodel: inherit\n---\n\n${raw.instructions.trim()}\n` },
      { id: 'readme', path: 'README.md', content: raw.readme.trim() + '\n\nИсточники использованного поиска:\n' + (sourceText || 'Источники не приложены.') + '\n' },
      { id: 'verification', path: 'verification.json', content: JSON.stringify(criteria, null, 2) + '\n' },
      ...criteria.files.map((file, index) => ({ id: 'fixture-' + index, path: join('fixtures', file.name), content: file.content })),
    ];
    for (const file of contents) { await writeFile(join(directory, file.path), file.content, { flag: 'wx', mode: 0o600 }); artifact.files.push({ id: file.id, path: file.path, hash: digest(file.content) }); }
    chat.artifacts.push(artifact);
    try { await store.save(chat); } catch (error) { chat.artifacts.splice(chat.artifacts.indexOf(artifact), 1); throw error; }
    return artifact;
  } catch (error) { await rm(directory, { recursive: true, force: true }); throw error; }
}
export async function artifactStatus(store: Store, chat: Chat, artifact: Artifact): Promise<{ status: 'unverified' | 'passed' | 'failed' | 'unavailable' | 'stale'; reason?: string }> {
  if (artifact.specHash !== specHash(chat.task)) return { status: 'stale', reason: 'Задача изменена' };
  let directory: string; let criteria: Verification; const hashes: Record<string, string> = {};
  try {
    if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(artifact.id) || artifact.path !== join('artifacts', artifact.id)) throw new Error('Неверный каталог комплекта');
    directory = await contained(store.root, artifact.path);
    for (const file of artifact.files) {
      if (hashes[file.id] !== undefined || !file.hash) throw new Error('Неверные hashes');
      hashes[file.id] = digest(await readFile(await contained(directory, file.path)));
      if (hashes[file.id] !== file.hash) throw new Error('Файлы комплекта изменены');
    }
    const verification = artifact.files.find(file => file.id === 'verification');
    if (!verification || !artifact.files.some(file => file.id === 'agent') || !artifact.files.some(file => file.id === 'readme')) throw new Error('Неполный комплект');
    criteria = parseVerification(JSON.parse(await readFile(await contained(directory, verification.path), 'utf8')));
    if (criteria.files.some((file, index) => !artifact.files.some(entry => entry.id === 'fixture-' + index && entry.path === join('fixtures', file.name)))) throw new Error('Нет тестовых файлов');
  } catch (error) { return { status: 'stale', reason: (error as Error).message }; }
  let report: unknown;
  try { if (artifact.validation !== 'validation.json') throw new Error('Нет отчёта'); report = JSON.parse(await readFile(await contained(directory, artifact.validation), 'utf8')); }
  catch { return { status: 'unverified', reason: 'Отчёт отсутствует или повреждён' }; }
  if (!object(report) || report.specHash !== artifact.specHash || !object(report.fileHashes) || Object.keys(hashes).length !== Object.keys(report.fileHashes).length || Object.entries(hashes).some(([id, hash]) => report.fileHashes && (report.fileHashes as Record<string, unknown>)[id] !== hash)) return { status: 'unverified', reason: 'Отчёт не соответствует комплекту' };
  if (report.status === 'failed' || report.status === 'unavailable') return { status: report.status, ...(typeof report.reason === 'string' ? { reason: report.reason } : {}) };
  if (report.status !== 'passed' || !Array.isArray(report.cases) || report.cases.length !== 3 || criteria.cases.some(item => item.manual || !report.cases || !(report.cases as unknown[]).some(result => object(result) && result.id === item.id && result.status === 'passed'))) return { status: 'unverified', reason: 'Критерии не подтверждены' };
  return { status: 'passed' };
}
