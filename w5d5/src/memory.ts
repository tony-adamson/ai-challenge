import type { Task, MemoryEntry, Message } from './store.js';

type Kind = 'clarifications' | 'constraints' | 'terms';
const record = (value: unknown): value is Record<string, unknown> => value !== null && typeof value === 'object' && !Array.isArray(value);
export function applyPatch(task: Task, patch: unknown, current: Message): Task {
  if (current.role !== 'user' || !record(patch) || Object.keys(patch).some(key => !['setGoal', 'upsert', 'remove'].includes(key))) throw new Error('Память принимает только проверенный пользовательский patch');
  const next = structuredClone(task);
  const touched = new Set<string>();
  const kindOf = (value: unknown): Kind => {
    if (!['clarifications', 'constraints', 'terms'].includes(String(value))) throw new Error('Неизвестный раздел памяти');
    return value as Kind;
  };
  const evidence = (value: unknown, emptyGoal = false): MemoryEntry => {
    if (!record(value) || typeof value.key !== 'string' || typeof value.value !== 'string' || value.messageId !== current.id || typeof value.quote !== 'string' || !value.quote.trim() || !current.content.includes(value.quote)) throw new Error('Изменение памяти не подтверждено цитатой текущего сообщения');
    const key = value.key.trim().toLowerCase();
    if (!key || (!emptyGoal && !value.value.trim())) throw new Error('Пустой ключ или значение памяти');
    return { key, value: value.value.trim(), messageId: current.id, quote: value.quote };
  };
  const once = (kind: string, key: string): void => {
    const identity = kind + ':' + key;
    if (touched.has(identity)) throw new Error('Неоднозначное повторное изменение ключа памяти');
    touched.add(identity);
  };
  if (patch.setGoal !== undefined) {
    const entry = evidence(patch.setGoal, true);
    if (entry.key !== 'goal') throw new Error('Ключ цели должен быть goal');
    next.goal = entry.value ? entry : null;
  }
  if (patch.upsert !== undefined) {
    if (!Array.isArray(patch.upsert)) throw new Error('upsert должен быть массивом');
    for (const action of patch.upsert as unknown[]) {
      if (!record(action)) throw new Error('Неверный upsert');
      const kind = kindOf(action.kind); const entry = evidence(action.entry);
      once(kind, entry.key);
      const index = next[kind].findIndex(item => item.key.trim().toLowerCase() === entry.key);
      if (index < 0) next[kind].push(entry); else next[kind][index] = entry;
    }
  }
  if (patch.remove !== undefined) {
    if (!Array.isArray(patch.remove)) throw new Error('remove должен быть массивом');
    for (const action of patch.remove as unknown[]) {
      if (!record(action)) throw new Error('Неверный remove');
      const kind = kindOf(action.kind);
      const entry = evidence({ ...action, value: 'remove' });
      once(kind, entry.key);
      const index = next[kind].findIndex(item => item.key.trim().toLowerCase() === entry.key);
      if (index < 0) throw new Error('Удаляемый ключ памяти отсутствует');
      next[kind].splice(index, 1);
    }
  }
  return next;
}
