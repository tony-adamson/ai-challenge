import test from 'node:test';
import assert from 'node:assert/strict';
import type { Task, Message, MemoryEntry } from '../src/store.js';
import { applyPatch } from '../src/memory.js';

const current: Message = { id: 'm2', role: 'user', status: 'accepted', content: 'Разбирай документацию. Только чтение. Бюджет 10. RAG означает поиск. Отменяю цель и бюджет.' };
const entry = (key: string, value: string, quote: string): MemoryEntry => ({ key, value, messageId: 'm2', quote });
const initial = (): Task => ({ goal: null, clarifications: [], constraints: [{ key: 'budget', value: '5', messageId: 'm1', quote: 'Бюджет 5' }], terms: [] });

test('goal and all memory containers receive independently specified current-user evidence', () => {
  const result = applyPatch(initial(), {
    setGoal: entry('goal', 'Разбирать документацию', 'Разбирай документацию'),
    upsert: [
      { kind: 'clarifications', entry: entry('mode', 'Только чтение', 'Только чтение') },
      { kind: 'constraints', entry: entry(' Budget ', '10', 'Бюджет 10') },
      { kind: 'terms', entry: entry(' RAG ', 'поиск', 'RAG означает поиск') },
    ],
  }, current);
  assert.deepEqual(result, {
    goal: { key: 'goal', value: 'Разбирать документацию', messageId: 'm2', quote: 'Разбирай документацию' },
    clarifications: [{ key: 'mode', value: 'Только чтение', messageId: 'm2', quote: 'Только чтение' }],
    constraints: [{ key: 'budget', value: '10', messageId: 'm2', quote: 'Бюджет 10' }],
    terms: [{ key: 'rag', value: 'поиск', messageId: 'm2', quote: 'RAG означает поиск' }],
  });
});

test('normalized key replaces exactly one entry and does not mutate prior task', () => {
  const task = initial();
  const before = structuredClone(task);
  const result = applyPatch(task, { upsert: [{ kind: 'constraints', entry: entry(' BUDGET ', '10', 'Бюджет 10') }] }, current);
  assert.deepEqual(result.constraints, [{ key: 'budget', value: '10', messageId: 'm2', quote: 'Бюджет 10' }]);
  assert.deepEqual(task, before);
});

test('unchanged old evidence remains valid without requiring current-message provenance', () => {
  const task = initial();
  const result = applyPatch(task, { upsert: [{ kind: 'terms', entry: entry('rag', 'поиск', 'RAG означает поиск') }] }, current);
  assert.deepEqual(result.constraints, [{ key: 'budget', value: '5', messageId: 'm1', quote: 'Бюджет 5' }]);
});

test('explicit cancellation clears goal and removes a normalized existing key', () => {
  const task = initial();
  task.goal = { key: 'goal', value: 'Исследовать', messageId: 'm1', quote: 'Исследовать' };
  const result = applyPatch(task, { setGoal: entry('goal', '', 'Отменяю цель'), remove: [{ kind: 'constraints', key: ' BUDGET ', messageId: 'm2', quote: 'Отменяю цель и бюджет' }] }, current);
  assert.equal(result.goal, null);
  assert.deepEqual(result.constraints, []);
});

test('assistant, stale message ID and nonliteral or empty quotes cannot supply new evidence', () => {
  const patch = { setGoal: entry('goal', 'Документация', 'Разбирай документацию') };
  assert.throws(() => applyPatch(initial(), patch, { ...current, role: 'assistant' }));
  assert.throws(() => applyPatch(initial(), { setGoal: { ...patch.setGoal, messageId: 'm1' } }, current));
  assert.throws(() => applyPatch(initial(), { setGoal: entry('goal', 'Документация', 'Несуществующая цитата') }, current));
  assert.throws(() => applyPatch(initial(), { setGoal: entry('goal', 'Документация', '') }, current));
});

test('invalid mixed patch never mutates task and empty non-goal values are rejected', () => {
  const task = initial();
  const before = structuredClone(task);
  const patch = { upsert: [{ kind: 'constraints', entry: entry('budget', '10', 'Бюджет 10') }, { kind: 'terms', entry: entry('rag', '', 'RAG означает поиск') }] };
  assert.throws(() => applyPatch(task, patch, current));
  assert.deepEqual(task, before);
});

test('missing removal key, forged removal evidence, duplicate normalized keys and unknown kind fail', () => {
  assert.throws(() => applyPatch(initial(), { remove: [{ kind: 'terms', key: 'missing', messageId: 'm2', quote: 'Отменяю цель' }] }, current));
  assert.throws(() => applyPatch(initial(), { remove: [{ kind: 'constraints', key: 'budget', messageId: 'm1', quote: 'Бюджет 5' }] }, current));
  assert.throws(() => applyPatch(initial(), { upsert: [{ kind: 'terms', entry: entry('RAG', 'поиск', 'RAG означает поиск') }, { kind: 'terms', entry: entry(' rag ', 'поиск', 'RAG означает поиск') }] }, current));
  assert.throws(() => applyPatch(initial(), { upsert: [{ kind: 'sources', entry: entry('rag', 'поиск', 'RAG означает поиск') }] }, current));
});
