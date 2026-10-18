// Токен приходит в ссылке запуска; каждый запрос к API несёт его в заголовке.
const TOKEN = new URLSearchParams(location.search).get('t') || '';
const $ = (id) => document.getElementById(id);
// ДОПУЩЕНИЕ-7: у каждого клиента своя лента чата; сервер так же держит свою историю на клиента.
const chats = {};

async function api(path, body) {
  const res = await fetch(path, {
    method: body ? 'POST' : 'GET',
    headers: { 'x-emi-token': TOKEN, 'Content-Type': 'application/json' },
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!res.ok) throw new Error(`HTTP ${res.status}: ${await res.text()}`);
  return res.json();
}

// Только textContent: ответ модели, цитаты и данные CRM — данные, не разметка.
function el(tag, cls, text) {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
}

async function loadCard(user) {
  try {
    const card = await api('api/customer?id=' + encodeURIComponent(user));
    if (user !== $('user').value) return;
    $('profile').textContent = card.get_profile;
    $('tickets').textContent = card.list_tickets;
  } catch (e) {
    if (user !== $('user').value) return;
    $('profile').textContent = 'не загрузилось: ' + e.message;
    $('tickets').textContent = '';
  }
}

function chatOf(user) {
  if (!chats[user]) {
    const box = el('div', 'chat');
    const name = $('user').selectedOptions[0].dataset.name;
    const hello = el('div', 'message emi');
    hello.append(el('div', 'label', 'ЭМИ'), el('div', 'content', `Здравствуйте, ${name}! Я Эми, ассистент поддержки «Ветрило». Чем помочь?`));
    box.append(hello);
    $('messages').append(box);
    chats[user] = box;
  }
  return chats[user];
}

function switchUser() {
  const user = $('user').value;
  for (const [id, box] of Object.entries(chats)) box.hidden = id !== user;
  chatOf(user).hidden = false;
  $('profile').textContent = '…';
  $('tickets').textContent = '…';
  loadCard(user);
}

function addMessage(chat, who, cls) {
  const box = el('div', 'message ' + cls);
  box.append(el('div', 'label', who), el('div', 'content'));
  chat.append(box);
  box.scrollIntoView({ block: 'end' });
  return box;
}

function quotesOf(list) {
  const quotes = el('div', 'quotes');
  for (const q of list) {
    const item = el('div', 'quote');
    item.append(el('div', 'src', q.label), el('div', 'text', '«' + q.text + '»'));
    quotes.append(item);
  }
  return quotes;
}

function render(box, reply) {
  box.className = 'message emi' + (reply.kind === 'error' ? ' error' : '');
  box.querySelector('.content').textContent = reply.kind === 'answer' ? reply.answer : reply.text;
  if (reply.quotes && reply.quotes.length) box.append(quotesOf(reply.quotes));
  const meta = el('div', 'meta');
  if (reply.providers) meta.append(el('span', '', 'модель: ' + reply.providers.join(' → ')));
  if (reply.fallback) meta.append(el('span', '', 'fallback: ' + reply.fallback.slice(0, 160)));
  for (const t of reply.tools || []) meta.append(el('span', '', 'MCP ' + t.name + ' ' + JSON.stringify(t.args)));
  if (reply.kind === 'answer') meta.append(el('span', '', reply.known ? 'цитаты проверены кодом' + (reply.retried ? ' (со 2-й попытки)' : '') : 'нет в базе знаний'));
  if (meta.childElementCount) box.append(meta);
  box.scrollIntoView({ block: 'end' });
}

$('ask').addEventListener('submit', async (event) => {
  event.preventDefault();
  const button = $('ask').querySelector('button');
  // requestSubmit() по Enter обходит disabled у кнопки — второй запрос, пока идёт первый, не шлём.
  if (button.disabled) return;
  const text = $('input').value.trim();
  const user = $('user').value;
  if (!text || !user) return;
  $('input').value = '';
  const chat = chatOf(user);
  addMessage(chat, 'КЛИЕНТ', 'user').querySelector('.content').textContent = text;
  const box = addMessage(chat, 'ЭМИ', 'emi pending');
  button.disabled = true;
  $('user').disabled = true;
  try {
    render(box, await api('api/chat', { user, text }));
  } catch (e) {
    render(box, { kind: 'error', text: e.message });
  } finally {
    button.disabled = false;
    $('user').disabled = false;
    $('input').focus();
    // Ассистент мог создать или дописать тикет — карточка перечитывается из CRM после каждого ответа.
    loadCard(user);
  }
});

$('input').addEventListener('keydown', (event) => {
  if (event.key === 'Enter' && !event.shiftKey) {
    event.preventDefault();
    $('ask').requestSubmit();
  }
});

$('user').addEventListener('change', switchUser);
$('refresh').addEventListener('click', () => loadCard($('user').value));

(async () => {
  try {
    const users = await api('api/users');
    if (users.error) throw new Error(users.error);
    for (const u of users) {
      const option = el('option', '', `${u.name} · ${u.status}`);
      option.value = u.id;
      option.dataset.name = u.name;
      $('user').append(option);
    }
    switchUser();
  } catch (e) {
    $('profile').textContent = 'клиенты не загрузились: ' + e.message;
  }
})();

// День 34: вкладка оператора. Чат клиента остаётся в DOM — переключение вкладок его не сбрасывает.
for (const tab of document.querySelectorAll('#tabs button')) {
  tab.addEventListener('click', () => {
    for (const t of document.querySelectorAll('#tabs button')) t.classList.toggle('active', t === tab);
    $('client').hidden = tab.dataset.tab !== 'client';
    $('operator').hidden = tab.dataset.tab !== 'operator';
  });
}

// Строки diff — только textContent, цвет даёт класс строки.
function showDiff(diff) {
  $('kb-diff').textContent = '';
  for (const line of (diff || '').split('\n')) {
    let cls = '';
    if (line.startsWith('+++') || line.startsWith('---') || line.startsWith('diff ')) cls = 'file';
    else if (line.startsWith('@@')) cls = 'hunk';
    else if (line.startsWith('+')) cls = 'add';
    else if (line.startsWith('-')) cls = 'del';
    $('kb-diff').append(cls ? el('span', cls, line) : document.createTextNode(line), document.createTextNode(cls ? '' : '\n'));
  }
  if (!diff) $('kb-diff').textContent = 'правок нет';
}

$('kb-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const button = $('kb-form').querySelector('button');
  if (button.disabled) return;
  button.disabled = true;
  $('kb-apply').disabled = true;
  $('kb-steps').textContent = '';
  $('kb-diff').textContent = '';
  $('kb-applied').textContent = '';
  const report = $('kb-report');
  report.className = 'message emi pending';
  report.querySelector('.content').textContent = '';
  report.querySelectorAll('.quotes, .meta').forEach((n) => n.remove());
  $('kb-status').textContent = 'Эми работает с файлами — это занимает до пары минут…';
  let reply;
  try {
    reply = await api('api/kb/task', { goal: $('goal').value });
  } catch (e) {
    reply = { kind: 'error', text: e.message };
  }
  button.disabled = false;
  $('kb-status').textContent = '';
  for (const t of reply.tools || []) $('kb-steps').append(el('li', t.ok ? '' : 'fail', t.name + ' ' + JSON.stringify(t.args) + (t.ok ? '' : ' — ошибка')));
  report.className = 'message emi' + (reply.kind === 'error' ? ' error' : '');
  report.querySelector('.content').textContent = reply.kind === 'report' ? reply.report : reply.text;
  if (reply.quotes && reply.quotes.length) report.append(quotesOf(reply.quotes));
  const meta = el('div', 'meta');
  if (reply.providers) meta.append(el('span', '', 'модель: ' + reply.providers.join(' → ')));
  if (reply.fallback) meta.append(el('span', '', 'fallback: ' + reply.fallback.slice(0, 160)));
  if (reply.kind === 'report' && reply.known) meta.append(el('span', '', 'цитаты и номера тикетов проверены кодом' + (reply.retried ? ' (со 2-й попытки)' : '')));
  if (meta.childElementCount) report.append(meta);
  showDiff(reply.diff);
  // Сервер запишет черновик, только если он совпадает с этим diff — тем, что оператор видел.
  shownDiff = reply.diff || '';
  $('kb-apply').disabled = !reply.pending;
});

let shownDiff = '';

$('kb-apply').addEventListener('click', async () => {
  $('kb-apply').disabled = true;
  try {
    const reply = await api('api/kb/apply', { diff: shownDiff });
    $('kb-applied').textContent = reply.text;
  } catch (e) {
    $('kb-applied').textContent = 'не применилось: ' + e.message;
  }
});
