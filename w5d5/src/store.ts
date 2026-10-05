import { mkdir, open, readFile, readdir, rename, unlink, realpath } from 'node:fs/promises';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';

export interface Settings { provider: 'cerebras' | 'deepseek' | 'openrouter'; model: string; temperature: number; reasoning: string }
export interface MemoryEntry { key: string; value: string; messageId: string; quote: string }
export interface Task { goal: MemoryEntry | null; clarifications: MemoryEntry[]; constraints: MemoryEntry[]; terms: MemoryEntry[] }
export interface Source { id: string; type: 'book' | 'document' | 'research' | 'deepwiki'; title: string; text: string; path?: string; url?: string; pages?: string; section?: string }
export interface Message { id: string; role: 'user' | 'assistant'; content: string; status: 'memory_pending' | 'memory_failed' | 'pending' | 'accepted' | 'failed' | 'interrupted'; replyTo?: string; retrieval?: Source[]; citations?: { id: string; quote: string }[]; retrievalError?: string; toolTraces?: Record<string, unknown>[]; error?: string }
export interface Artifact { id: string; path: string; specHash: string; files: { id: string; path: string; hash?: string }[]; validation?: string }
export interface Chat { schemaVersion: 1; id: string; settings: Settings; messages: Message[]; task: Task; artifacts: Artifact[] }
export class HttpError extends Error { constructor(public status: number, message: string) { super(message); } }
export const validId = (value: string): boolean => /^[a-zA-Z0-9_-]{1,100}$/.test(value);
const object = (value: unknown): value is Record<string, unknown> => value !== null && typeof value === 'object' && !Array.isArray(value);
const strings = (value: unknown, keys: string[]): boolean => object(value) && keys.every(key => typeof value[key] === 'string');
export function validateSettings(value: unknown): asserts value is Settings {
  if (!strings(value, ['provider', 'model', 'reasoning']) || !object(value) || !['cerebras', 'deepseek', 'openrouter'].includes(String(value.provider)) || !String(value.model).trim() || typeof value.temperature !== 'number' || !Number.isFinite(value.temperature) || value.temperature < 0 || value.temperature > 2 || Object.keys(value).some(key => !['provider', 'model', 'reasoning', 'temperature'].includes(key))) throw new HttpError(400, 'Invalid settings');
}
function validateChat(value: unknown): asserts value is Chat {
  if (!object(value) || value.schemaVersion !== 1 || typeof value.id !== 'string' || !validId(value.id) || !Array.isArray(value.messages) || !Array.isArray(value.artifacts) || !object(value.task)) throw new Error('Invalid chat snapshot');
  validateSettings(value.settings);
  const entry = (item: unknown) => strings(item, ['key', 'value', 'messageId', 'quote']);
  const task = value.task;
  if (task.goal !== null && !entry(task.goal)) throw new Error('Invalid task goal');
  for (const kind of ['clarifications', 'constraints', 'terms']) {
    const entries = task[kind];
    if (!Array.isArray(entries) || !entries.every(entry) || new Set(entries.map(item => String(item.key).trim().toLowerCase())).size !== entries.length) throw new Error('Invalid task entries');
  }
  const messages = value.messages;
  if (!messages.every(item => strings(item, ['id', 'role', 'content', 'status']) && object(item) && validId(String(item.id)) && ['user', 'assistant'].includes(String(item.role)) && ['memory_pending', 'memory_failed', 'pending', 'accepted', 'failed', 'interrupted'].includes(String(item.status)) && (item.retrieval === undefined || Array.isArray(item.retrieval) && item.retrieval.every(source => strings(source, ['id', 'text', 'type', 'title']) && object(source) && ['book', 'document', 'research', 'deepwiki'].includes(String(source.type)))) && (item.toolTraces === undefined || Array.isArray(item.toolTraces) && item.toolTraces.every(object)))) throw new Error('Invalid messages');
  if (new Set(messages.map(item => item.id)).size !== messages.length) throw new Error('Duplicate message IDs');
  if (messages.some(item => item.replyTo !== undefined && (item.role !== 'assistant' || !messages.some(parent => parent.role === 'user' && parent.id === item.replyTo)) || item.citations !== undefined && (!Array.isArray(item.citations) || !item.citations.every((citation: unknown) => strings(citation, ['id', 'quote']))))) throw new Error('Invalid response source references');
  for (const item of [task.goal, ...task.clarifications as unknown[], ...task.constraints as unknown[], ...task.terms as unknown[]]) {
    if (item === null) continue;
    const evidence = item as MemoryEntry;
    if (!evidence.key.trim() || !evidence.quote || !messages.some(message => message.id === evidence.messageId && message.role === 'user' && String(message.content).includes(evidence.quote))) throw new Error('Invalid user evidence');
  }
  if (!value.artifacts.every(item => strings(item, ['id', 'path', 'specHash']) && object(item) && validId(String(item.id)) && Array.isArray(item.files) && item.files.every(file => strings(file, ['id', 'path'])))) throw new Error('Invalid artifacts');
}

export class Store {
  private closed = false;
  private writes: Promise<void> = Promise.resolve();
  private constructor(public readonly root: string, private readonly ownership: string, private readonly recoverIds: Set<string>) {}
  static async open(root: string): Promise<Store> {
    await mkdir(join(root, 'chats'), { recursive: true });
    root = await realpath(root);
    const lock = join(root, 'writer.lock');
    const ownership = JSON.stringify({ pid: process.pid, token: randomUUID() });
    const acquire = async () => {
      const handle = await open(lock, 'wx', 0o600);
      try { await handle.writeFile(ownership); } finally { await handle.close(); }
      return new Store(root, ownership, new Set((await readdir(join(root, 'chats'))).filter(name => name.endsWith('.json')).map(name => name.slice(0, -5))));
    };
        // Publish a unique PID guard before inspecting peers: a later contender sees us,
        // while an earlier contender is seen by us. Dead guards are never reused, so
        // cleanup after a crash cannot remove another process's newly created guard.
        const guard = `writer.lock.reclaim.${process.pid}.${randomUUID()}`;
        const reclaim = await open(join(root, guard), 'wx', 0o600);
        await reclaim.close();
        try {
          const guards = (await readdir(root)).filter(name => name.startsWith('writer.lock.reclaim.'));
          if (guards.length > 1000) throw new Error('Too many writer reclaim guards');
          for (const peer of guards) {
            if (peer === guard) continue;
            const match = /^writer\.lock\.reclaim\.([1-9][0-9]*)\.[a-zA-Z0-9_-]+$/.exec(peer);
            if (!match || !Number.isSafeInteger(Number(match[1]))) throw new Error('Invalid reclaim guard');
            try { process.kill(Number(match[1]), 0); throw new Error('Writer reclamation in progress'); }
            catch (failure) { if ((failure as NodeJS.ErrnoException).code !== 'ESRCH') throw failure; }
            await unlink(join(root, peer)).catch(failure => { if ((failure as NodeJS.ErrnoException).code !== 'ENOENT') throw failure; });
          }
          let previous: string;
          try { previous = await readFile(lock, 'utf8'); } catch (failure) { if ((failure as NodeJS.ErrnoException).code === 'ENOENT') return await acquire(); throw failure; }
          const owner: unknown = JSON.parse(previous);
          if (!object(owner) || !Number.isSafeInteger(owner.pid) || Number(owner.pid) < 1) throw new Error('Invalid writer lock');
          try { process.kill(Number(owner.pid), 0); throw new Error('Data root has a live writer'); }
          catch (failure) { if ((failure as NodeJS.ErrnoException).code !== 'ESRCH') throw failure; }
          if (await readFile(lock, 'utf8') === previous) await unlink(lock);
          return await acquire();
        } finally { await unlink(join(root, guard)); }
  }
  private file(id: string): string { if (this.closed) throw new Error('Store closed'); if (!validId(id)) throw new HttpError(404, 'Chat not found'); return join(this.root, 'chats', `${id}.json`); }
  async create(settings: Settings): Promise<Chat> {
    validateSettings(settings);
    const chat: Chat = { schemaVersion: 1, id: randomUUID(), settings: structuredClone(settings), messages: [], task: { goal: null, clarifications: [], constraints: [], terms: [] }, artifacts: [] };
    await this.save(chat); return chat;
  }
  async get(id: string): Promise<Chat> {
    let bytes: string;
    try { bytes = await readFile(this.file(id), 'utf8'); } catch (error) { if ((error as NodeJS.ErrnoException).code === 'ENOENT') throw new HttpError(404, 'Chat not found'); throw error; }
    const chat: unknown = JSON.parse(bytes); validateChat(chat);
    if (chat.id !== id) throw new Error('Snapshot ID mismatch');
    let recovered = false;
    for (const message of this.recoverIds.has(id) ? chat.messages : []) {
      if (message.status === 'pending' || message.status === 'memory_pending') { message.status = message.status === 'pending' ? 'interrupted' : 'memory_failed'; recovered = true; }
    }
    if (recovered) await this.save(chat);
    this.recoverIds.delete(id);
    return chat;
  }
  async list(): Promise<Chat[]> { return Promise.all((await readdir(join(this.root, 'chats'))).filter(name => name.endsWith('.json')).map(name => this.get(name.slice(0, -5)))); }
  async save(chat: Chat): Promise<void> {
    validateChat(chat); const destination = this.file(chat.id); const bytes = JSON.stringify(chat); this.recoverIds.delete(chat.id);
    const write = this.writes.then(async () => {
      const temporary = `${destination}.${randomUUID()}.tmp`;
      try { const handle = await open(temporary, 'wx', 0o600); try { await handle.writeFile(bytes); await handle.sync(); } finally { await handle.close(); } await rename(temporary, destination); }
      finally { await unlink(temporary).catch(error => { if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error; }); }
    });
    this.writes = write.catch(() => {}); await write;
  }
  async close(): Promise<void> { if (this.closed) return; this.closed = true; await this.writes; const lock = join(this.root, 'writer.lock'); if (await readFile(lock, 'utf8') === this.ownership) await unlink(lock); }
}
