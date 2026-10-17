// Токен приходит в ссылке запуска; каждый запрос к API несёт его в заголовке.
const TOKEN = new URLSearchParams(location.search).get('t') || '';
const $ = (id) => document.getElementById(id);

async function api(path, body) {
  const res = await fetch(path, {
    method: body ? 'POST' : 'GET',
    headers: { 'x-emi-token': TOKEN, 'Content-Type': 'application/json' },
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!res.ok) throw new Error(`HTTP ${res.status}: ${await res.text()}`);
  return res.json();
}

async function loadCard() {
  try {
    const card = await api('/api/project');
    $('project-path').textContent = card.path;
    $('branch').textContent = card.branch;
    $('status').textContent = card.status;
    $('log').textContent = card.log;
  } catch (e) {
    $('status').textContent = 'не загрузилось: ' + e.message;
  }
}

// Только textContent: ответ модели и цитаты — данные, не разметка.
function el(tag, cls, text) {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
}

function addMessage(who, cls) {
  const box = el('div', 'message ' + cls);
  box.append(el('div', 'label', who), el('div', 'content'));
  $('messages').append(box);
  box.scrollIntoView({ block: 'end' });
  return box;
}

function render(box, reply) {
  box.className = 'message emi' + (reply.kind === 'error' ? ' error' : '');
  const content = box.querySelector('.content');
  content.textContent = reply.kind === 'help' ? reply.answer : reply.text;
  if (reply.kind === 'help' && reply.quotes && reply.quotes.length) {
    const quotes = el('div', 'quotes');
    for (const q of reply.quotes) {
      const item = el('div', 'quote');
      item.append(el('div', 'src', q.label), el('div', 'text', '«' + q.text + '»'));
      quotes.append(item);
    }
    box.append(quotes);
  }
  if (reply.hints && reply.hints.length) {
    box.append(el('div', 'content', 'Ближе всего по теме:\n' + reply.hints.map((h) => '• ' + h).join('\n')));
  }
  const meta = el('div', 'meta');
  if (reply.providers) meta.append(el('span', '', 'модель: ' + reply.providers.join(' → ')));
  if (reply.fallback) meta.append(el('span', '', 'fallback: ' + reply.fallback.slice(0, 160)));
  for (const t of reply.tools || []) meta.append(el('span', '', 'MCP ' + t.name + ' ' + JSON.stringify(t.args)));
  if (reply.kind === 'help') meta.append(el('span', '', reply.known ? 'цитаты проверены кодом' + (reply.retried ? ' (со 2-й попытки)' : '') : 'нет в документации'));
  if (meta.childElementCount) box.append(meta);
  box.scrollIntoView({ block: 'end' });
}

$('ask').addEventListener('submit', async (event) => {
  event.preventDefault();
  const button = $('ask').querySelector('button');
  // requestSubmit() по Enter обходит disabled у кнопки — второй запрос, пока идёт первый, не шлём.
  if (button.disabled) return;
  const text = $('input').value.trim();
  if (!text) return;
  $('input').value = '';
  addMessage('ТЫ', 'user').querySelector('.content').textContent = text;
  const box = addMessage('ЭМИ', 'emi pending');
  button.disabled = true;
  try {
    render(box, await api('/api/chat', { text }));
  } catch (e) {
    render(box, { kind: 'error', text: e.message });
  } finally {
    button.disabled = false;
    $('input').focus();
  }
});

$('input').addEventListener('keydown', (event) => {
  if (event.key === 'Enter' && !event.shiftKey) {
    event.preventDefault();
    $('ask').requestSubmit();
  }
});

$('refresh').addEventListener('click', loadCard);
loadCard();
