import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { request as httpRequest } from 'node:http';
import { setTimeout as pause } from 'node:timers/promises';
import { startServer } from '../src/server.js';

const settings = { provider: 'deepseek', model: 'deepseek-chat', temperature: 0.3, reasoning: 'none' } as const;

test('delayed request body cannot overwrite a completed newer turn', { timeout: 10_000 }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'w5d5-delayed-'));
  const server = await startServer({ root, action: async (_op, chat, input) => {
    chat.messages.push({ id: String(input.content), role: 'user', content: String(input.content), status: 'accepted' });
    await server.store.save(chat);
  } });
  let delayed: ReturnType<typeof httpRequest> | undefined;
  try {
    const session = await fetch(server.origin + '/api/session', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ token: new URL(server.bootstrapUrl).hash.slice(1) }) });
    const Cookie = session.headers.get('set-cookie')!.split(';')[0]!;
    const chat = await server.store.create(settings);
    const base = `${server.origin}/api/chats/${chat.id}`;
    const wait = async (operationId: string) => {
      for (let attempt = 0; attempt < 500; attempt++) {
        const operation = await fetch(`${base}/operations/${operationId}`, { headers: { Cookie } }).then(response => response.json()) as { status: string };
        if (operation.status !== 'running') { assert.equal(operation.status, 'completed'); return; }
        await pause(10);
      }
      assert.fail('Operation did not finish');
    };
    const finished = new Promise<{ operationId: string }>((resolve, reject) => {
      delayed = httpRequest(base + '/ask', { method: 'POST', headers: { Cookie, 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(JSON.stringify({ content: 'B' })) } }, response => {
        let bytes = ''; response.on('data', chunk => { bytes += String(chunk); });
        response.on('end', () => { try { assert.equal(response.statusCode, 202); resolve(JSON.parse(bytes) as { operationId: string }); } catch (error) { reject(error); } });
      });
      delayed.on('error', reject); delayed.write('{"content":');
    });
    await pause(250);
    const first = await fetch(base + '/ask', { method: 'POST', headers: { Cookie, 'Content-Type': 'application/json' }, body: JSON.stringify({ content: 'A' }) });
    assert.equal(first.status, 202); await wait((await first.json() as { operationId: string }).operationId);
    delayed!.end('"B"}'); await wait((await finished).operationId);
    const saved = await fetch(base, { headers: { Cookie } }).then(response => response.json()) as { messages: { content: string }[] };
    assert.deepEqual(saved.messages.map(message => message.content), ['A', 'B']);
  } finally { delayed?.destroy(); await server.close(); await rm(root, { recursive: true, force: true }); }
});

test('saved HTML source is authorized JSON data; oversized mutation is rejected', async () => {
  const root = await mkdtemp(join(tmpdir(), 'w5d5-source-'));
  const server = await startServer({ root });
  try {
    const token = new URL(server.bootstrapUrl).hash.slice(1);
    const session = await fetch(server.origin + '/api/session', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ token }) });
    const Cookie = session.headers.get('set-cookie')!.split(';')[0]!;
    const chat = await server.store.create(settings);
    const source = { id: 'html-source', type: 'document', title: 'HTML fixture', text: '<script>globalThis.executed=true</script>', path: join(root, 'fixture.html') } as const;
    await writeFile(source.path, source.text);
    chat.messages.push({ id: 'm1', role: 'user', content: 'Покажи источник', status: 'accepted', retrieval: [source] });
    await server.store.save(chat);
    const url = `${server.origin}/api/chats/${chat.id}/sources/m1/${source.id}`;
    assert.equal((await fetch(url)).status, 401);
    const response = await fetch(url, { headers: { Cookie } });
    assert.equal(response.status, 200);
    assert.match(response.headers.get('content-type')!, /^application\/json/);
    assert.equal(response.headers.get('x-content-type-options'), 'nosniff');
    assert.equal((await response.json() as { text: string }).text, source.text);
    const tooLarge = await fetch(`${server.origin}/api/chats/${chat.id}/ask`, { method: 'POST', headers: { Cookie, 'Content-Type': 'application/json' }, body: JSON.stringify({ content: 'x'.repeat(65_536) }) });
    assert.equal(tooLarge.status, 413);
  } finally { await server.close(); await rm(root, { recursive: true, force: true }); }
});

test('actual HTTP enforces sessions, exact Host/Origin, one-use bootstrap and opaque IDs', async () => {
  const root = await mkdtemp(join(tmpdir(), 'w5d5-http-'));
  await writeFile(join(root, 'payload.html'), '<script>globalThis.executed=true</script>');
  const server = await startServer({ root, staticRoot: root });
  const request = (url: string, init?: RequestInit) => fetch(server.origin + url, init);
  try {
    assert.equal((await request('/api/chats')).status, 401);
    assert.equal((await request('/api/chats', { headers: { Origin: 'https://evil.example' } })).status, 403);
    const foreignHostStatus = await new Promise<number | undefined>((resolve, reject) => {
      const outgoing = httpRequest(server.origin + '/api/chats', { headers: { Host: 'evil.example' } }, response => { response.resume(); resolve(response.statusCode); });
      outgoing.on('error', reject);
      outgoing.end();
    });
    assert.equal(foreignHostStatus, 403);
    const token = new URL(server.bootstrapUrl).hash.slice(1);
    const exchange = () => request('/api/session', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ token }) });
    const session = await exchange();
    assert.equal(session.status, 200);
    const cookieHeader = session.headers.get('set-cookie')!;
    assert.match(cookieHeader, /HttpOnly/i);
    assert.match(cookieHeader, /SameSite=Strict/i);
    const Cookie = cookieHeader.split(';')[0]!;
    assert.equal((await exchange()).status, 401);
    const created = await request('/api/chats', { method: 'POST', headers: { Cookie, 'Content-Type': 'application/json' }, body: JSON.stringify({ settings }) });
    assert.equal(created.status, 201);
    const chat = await created.json() as { id: string };
    assert.equal((await request(`/api/chats/${chat.id}`, { headers: { Cookie } })).status, 200);
    assert.equal((await request('/api/chats/missing', { headers: { Cookie } })).status, 404);
    assert.equal((await request(`/api/chats/${chat.id}/operations/missing`, { headers: { Cookie } })).status, 404);
    assert.equal((await request('/payload.html')).status, 404);
  } finally { await server.close(); await rm(root, { recursive: true, force: true }); }
});

test('busy chat rejects another action; cancellation kills owned child and repeat is harmless', { timeout: 10_000 }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'w5d5-cancel-'));
  let childPid = 0;
  let started!: () => void;
  const ready = new Promise<void>(resolve => { started = resolve; });
  let exited!: () => void;
  const childExited = new Promise<void>(resolve => { exited = resolve; });
  const server = await startServer({ root, action: async (_op, _chat, _input, signal) => {
    const child = spawn(process.execPath, ['-e', 'setInterval(()=>{},1000)'], { signal, stdio: 'ignore' });
    child.on('error', () => {});
    child.once('exit', exited);
    childPid = child.pid!;
    started();
    await once(child, 'close');
    signal.throwIfAborted();
    return { text: 'must not be published after cancellation' };
  } });
  try {
    const token = new URL(server.bootstrapUrl).hash.slice(1);
    const session = await fetch(server.origin + '/api/session', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ token }) });
    const Cookie = session.headers.get('set-cookie')!.split(';')[0]!;
    const chat = await server.store.create(settings);
    const base = `${server.origin}/api/chats/${chat.id}`;
    const post = (suffix: string) => fetch(base + suffix, { method: 'POST', headers: { Cookie, 'Content-Type': 'application/json' }, body: JSON.stringify({ content: 'Вопрос' }) });
    const accepted = await post('/ask');
    assert.equal(accepted.status, 202);
    const { operationId } = await accepted.json() as { operationId: string };
    await ready;
    assert.equal((await post('/ask')).status, 409);
    const cancelled = await post(`/operations/${operationId}/cancel`);
    assert.equal(cancelled.status, 200);
    await childExited;
    assert.throws(() => process.kill(childPid, 0), /ESRCH/);
    assert.equal((await post(`/operations/${operationId}/cancel`)).status, 200);
    const operation = await fetch(`${base}/operations/${operationId}`, { headers: { Cookie } });
    assert.equal((await operation.json() as { status: string }).status, 'cancelled');
    assert.equal((await server.store.get(chat.id)).messages.some(message => message.role === 'assistant'), false);
  } finally { await server.close(); await rm(root, { recursive: true, force: true }); }
});
