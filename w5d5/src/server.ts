import { createServer, type IncomingMessage, type ServerResponse } from 'node:http';
import { randomBytes, randomUUID } from 'node:crypto';
import { readFile, realpath } from 'node:fs/promises';
import { resolve, sep, basename } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Store, HttpError, type Chat } from './store.js';
import { artifactStatus } from './artifacts.js';

type Action = (op: string, chat: Chat, input: Record<string, unknown>, signal: AbortSignal, progress: (stage: string) => void) => Promise<unknown>;
interface Operation { operationId: string; chatId: string; status: 'running' | 'completed' | 'failed' | 'cancelled'; progress?: string; result?: unknown; error?: string; controller: AbortController; done: Promise<void> }
async function body(request: IncomingMessage): Promise<Record<string, unknown>> {
  if (request.headers['content-type']?.split(';')[0] !== 'application/json') throw new HttpError(400, 'JSON required');
  const chunks: Buffer[] = []; let size = 0;
  for await (const chunk of request) { const bytes = Buffer.from(chunk as Uint8Array); size += bytes.length; if (size > 65536) throw new HttpError(413, 'Body too large'); chunks.push(bytes); }
  try { const value: unknown = JSON.parse(Buffer.concat(chunks).toString('utf8')); if (value === null || typeof value !== 'object' || Array.isArray(value)) throw new Error(); return value as Record<string, unknown>; }
  catch { throw new HttpError(400, 'Malformed JSON'); }
}
function json(response: ServerResponse, status: number, value: unknown): void { response.writeHead(status, { 'Content-Type': 'application/json; charset=utf-8', 'X-Content-Type-Options': 'nosniff', 'Cache-Control': 'no-store' }); response.end(JSON.stringify(value)); }
async function contained(root: string, file: string): Promise<string> {
  const base = await realpath(root); const target = await realpath(resolve(base, file));
  if (!target.startsWith(base + sep)) throw new HttpError(403, 'File outside allowed root'); return target;
}
export async function startServer(options: { root: string; staticRoot?: string; action?: Action }) {
  const store = await Store.open(options.root);
  const token = randomBytes(32).toString('hex'); const expires = Date.now() + 60000; let used = false; let closing = false;
  const sessions = new Set<string>(); const operations = new Map<string, Operation>(); const busy = new Set<string>();
  const staticRoot = options.staticRoot ?? fileURLToPath(new URL('../../static/', import.meta.url));
  let origin = '';
  const server = createServer((request, response) => { void handle(request, response).catch(error => {
    if (!response.headersSent) json(response, error instanceof HttpError ? error.status : 500, { error: error instanceof HttpError ? error.message : 'Local operation failed' }); else response.end();
  }); });
  server.requestTimeout = 60000; server.headersTimeout = 10000;
  async function handle(request: IncomingMessage, response: ServerResponse): Promise<void> {
    if (closing) throw new HttpError(503, 'Server closing');
    if (request.headers.host !== new URL(origin).host || request.headers.origin !== undefined && request.headers.origin !== origin || request.headers['sec-fetch-site'] === 'cross-site') throw new HttpError(403, 'Foreign request');
    const url = new URL(request.url ?? '/', origin); const method = request.method;
    if (!url.pathname.startsWith('/api/')) {
      const files: Record<string, [string, string]> = { '/': ['index.html', 'text/html'], '/app.js': ['app.js', 'text/javascript'], '/style.css': ['style.css', 'text/css'] };
      const item = files[url.pathname]; if (method !== 'GET' || !item) throw new HttpError(404, 'Resource not found');
      let bytes: Buffer; try { bytes = await readFile(await contained(staticRoot, item[0])); } catch { throw new HttpError(404, 'Resource not found'); }
      response.writeHead(200, { 'Content-Type': item[1] + '; charset=utf-8', 'X-Content-Type-Options': 'nosniff', 'Content-Security-Policy': "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'" }); response.end(bytes); return;
    }
    if (url.pathname === '/api/session' && method === 'POST') {
      const input = await body(request);
      if (used || Date.now() >= expires || input.token !== token) throw new HttpError(401, 'Invalid capability');
      used = true; const session = randomBytes(32).toString('hex'); sessions.add(session);
      response.setHeader('Set-Cookie', `session=${session}; HttpOnly; SameSite=Strict; Path=/`); json(response, 200, { status: 'authenticated' }); return;
    }
    const cookie = request.headers.cookie?.split(';').map(item => item.trim()).find(item => item.startsWith('session='))?.slice(8);
    if (!cookie || !sessions.has(cookie)) throw new HttpError(401, 'Session required');
    if (url.pathname === '/api/chats') {
      if (method === 'GET') { json(response, 200, await store.list()); return; }
      if (method === 'POST') { const input = await body(request); json(response, 201, await store.create(input.settings as Chat['settings'])); return; }
    }
    let parts: string[];
    try { parts = url.pathname.split('/').slice(1).map(part => decodeURIComponent(part)); } catch { throw new HttpError(400, 'Malformed URL'); }
    const chatId = parts[2];
    if (parts[0] !== 'api' || parts[1] !== 'chats' || !chatId) throw new HttpError(404, 'Route not found');
    let chat = await store.get(chatId);
    if (parts.length === 3 && method === 'GET') { json(response, 200, { ...chat, artifacts: await Promise.all(chat.artifacts.map(async artifact => ({ ...artifact, ...await artifactStatus(store, chat, artifact) }))) }); return; }
    if (parts[3] === 'operations') {
      const operation = operations.get(parts[4] ?? ''); if (!operation || operation.chatId !== chat.id) throw new HttpError(404, 'Operation not found');
      if (parts.length === 6 && parts[5] === 'cancel' && method === 'POST') {
        await body(request);
        if (operation.status === 'running') { operation.status = 'cancelled'; operation.controller.abort(); await operation.done; }
      } else if (parts.length !== 5 || method !== 'GET') throw new HttpError(404, 'Route not found');
      const { controller: _controller, done: _done, chatId: _chatId, ...publicOperation } = operation; json(response, 200, publicOperation); return;
    }
    if (parts[3] === 'sources' && parts.length === 6 && method === 'GET') {
      const message = chat.messages.find(item => item.id === parts[4]);
      const retrieval = message?.retrieval ?? chat.messages.find(item => item.id === message?.replyTo)?.retrieval;
      const source = retrieval?.find(item => item.id === parts[5]);
      if (!source) throw new HttpError(404, 'Source not found'); json(response, 200, source); return;
    }
    if (parts[3] === 'artifacts' && parts[5] === 'files' && parts.length === 7 && method === 'GET') {
      const artifact = chat.artifacts.find(item => item.id === parts[4]); const file = parts[6] === 'validation' && artifact?.validation === 'validation.json' ? { id: 'validation', path: artifact.validation } : artifact?.files.find(item => item.id === parts[6]);
      if (!artifact || !file) throw new HttpError(404, 'File not found');
      const bundle = await contained(store.root, artifact.path); const target = await contained(bundle, file.path); const bytes = await readFile(target);
      if (url.searchParams.get('download') === '1') { response.writeHead(200, { 'Content-Type': 'application/octet-stream', 'Content-Disposition': `attachment; filename="${basename(target).replace(/[^a-zA-Z0-9._-]/g, '_')}"`, 'X-Content-Type-Options': 'nosniff' }); response.end(bytes); }
      else json(response, 200, { id: file.id, name: basename(target), content: bytes.toString('utf8') }); return;
    }
    const action = parts[3];
    if (parts.length !== 4 || method !== 'POST' || !action || !['ask', 'retry', 'build', 'validate'].includes(action)) throw new HttpError(404, 'Route not found');
    const input = await body(request);
    if (busy.has(chat.id)) throw new HttpError(409, 'Chat busy');
    if (!options.action) throw new HttpError(503, 'Model actions unavailable');
    if (action === 'ask' && (typeof input.content !== 'string' || !input.content.trim())) throw new HttpError(400, 'Question required');
    busy.add(chat.id);
    try {
      // A slow request body may have outlived another completed operation.
      chat = await store.get(chat.id);
      if (closing) throw new HttpError(503, 'Server closing');
    } catch (error) { busy.delete(chat.id); throw error; }
    const operation: Operation = { operationId: randomUUID(), chatId: chat.id, status: 'running', controller: new AbortController(), done: Promise.resolve() };
    operations.set(operation.operationId, operation);
    const timeout = setTimeout(() => { operation.controller.abort(new Error('Operation timeout')); }, action === 'validate' ? 550000 : 300000);
    operation.done = (async () => {
      try { const result = await options.action!(action, chat, input, operation.controller.signal, stage => { if (operation.status === 'running') operation.progress = stage; }); operation.controller.signal.throwIfAborted(); if (operation.status === 'running') { operation.result = result; operation.status = 'completed'; } }
      catch (error) { if (operation.status === 'running') { operation.status = 'failed'; operation.error = error instanceof Error ? error.message : 'Action failed'; } }
      finally { clearTimeout(timeout); busy.delete(chat.id); }
    })();
    json(response, 202, { operationId: operation.operationId, status: operation.status });
  }
  try { await new Promise<void>((resolveReady, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolveReady); }); }
  catch (error) { await store.close(); throw error; }
  const address = server.address(); if (!address || typeof address === 'string') throw new Error('No local address');
  origin = `http://127.0.0.1:${address.port}`;
  return { origin, bootstrapUrl: `${origin}/#${token}`, store, close: async () => {
    if (closing) return; closing = true; sessions.clear();
    for (const operation of operations.values()) if (operation.status === 'running') { operation.status = 'cancelled'; operation.controller.abort(); }
    await Promise.all([...operations.values()].map(operation => operation.done));
    await new Promise<void>((done, reject) => server.close(error => error ? reject(error) : done())); await store.close();
  } };
}
