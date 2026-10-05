import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile, writeFile, rm, access } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Store } from '../src/store.js';

const settings = { provider: 'deepseek', model: 'deepseek-chat', temperature: 0.3, reasoning: 'none' } as const;

test('snapshot preserves Unicode history, settings and user evidence after reopen', async () => {
  const root = await mkdtemp(join(tmpdir(), 'w5d5-store-'));
  let store = await Store.open(root);
  try {
    const chat = await store.create(settings);
    chat.messages.push({ id: 'm1', role: 'user', content: 'Не менять исходники.\nТермин: café', status: 'accepted' });
    chat.task.constraints.push({ key: 'readonly', value: 'Не менять исходники', messageId: 'm1', quote: 'Не менять исходники' });
    await store.save(chat);
    await store.close();
    store = await Store.open(root);
    assert.deepEqual(await store.get(chat.id), chat);
    assert.equal((await store.list()).length, 1);
  } finally { await store.close(); await rm(root, { recursive: true, force: true }); }
});

test('corrupt snapshot remains byte-for-byte intact and fails explicitly', async () => {
  const root = await mkdtemp(join(tmpdir(), 'w5d5-corrupt-'));
  const store = await Store.open(root);
  try {
    const chat = await store.create(settings);
    const file = join(root, 'chats', `${chat.id}.json`);
    const broken = '{"schemaVersion":1,"messages":';
    await writeFile(file, broken);
    await assert.rejects(store.get(chat.id));
    assert.equal(await readFile(file, 'utf8'), broken);
  } finally { await store.close(); await rm(root, { recursive: true, force: true }); }
});

test('one live writer per root; closing releases ownership', async () => {
  const root = await mkdtemp(join(tmpdir(), 'w5d5-lock-'));
  const store = await Store.open(root);
  try { await assert.rejects(Store.open(root)); }
  finally { await store.close(); }
  const reopened = await Store.open(root);
  await reopened.close();
  await rm(root, { recursive: true, force: true });
});

test('restart recovers unfinished turns without accepting partial answers', async () => {
  const root = await mkdtemp(join(tmpdir(), 'w5d5-recovery-'));
  let store = await Store.open(root);
  try {
    const chat = await store.create(settings);
    chat.messages.push({ id: 'u1', role: 'user', content: 'Цель', status: 'memory_pending' });
    chat.messages.push({ id: 'a1', role: 'assistant', content: '', status: 'pending' });
    await store.save(chat);
    await store.close();
    store = await Store.open(root);
    assert.deepEqual((await store.get(chat.id)).messages.map(message => message.status), ['memory_failed', 'interrupted']);
  } finally { await store.close(); await rm(root, { recursive: true, force: true }); }
});

test('dead writer and crashed unique reclaimer release the root for a new writer', async () => {
  const root = await mkdtemp(join(tmpdir(), 'w5d5-reclaim-dead-'));
  const guard = join(root, 'writer.lock.reclaim.2147483647.deadfixture');
  await writeFile(join(root, 'writer.lock'), JSON.stringify({ pid: 2147483647, token: 'dead' }));
  await writeFile(guard, 'crashed');
  const store = await Store.open(root);
  try {
    const chat = await store.create(settings);
    await assert.rejects(access(guard), { code: 'ENOENT' });
    await store.close();
    const reopened = await Store.open(root);
    try { assert.deepEqual(await reopened.get(chat.id), chat); }
    finally { await reopened.close(); }
  } finally { await store.close(); await rm(root, { recursive: true, force: true }); }
});

test('a live foreign reclaimer blocks opening and retains its guard unchanged', async () => {
  const root = await mkdtemp(join(tmpdir(), 'w5d5-reclaim-live-'));
  const guard = join(root, `writer.lock.reclaim.${process.pid}.foreign`);
  await writeFile(guard, 'foreign-owner');
  try {
    await assert.rejects(Store.open(root));
    assert.equal(await readFile(guard, 'utf8'), 'foreign-owner');
  } finally { await rm(root, { recursive: true, force: true }); }
});
