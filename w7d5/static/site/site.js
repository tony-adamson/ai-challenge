// Заглушка сайта и кабинета «Ветрило». Все пути относительные: на сервере сайт живёт под префиксом /vetrilo/ (ДОПУЩЕНИЕ-4).
const $ = (id) => document.getElementById(id);
const STATUS = { active: 'активна', trial: 'пробный период', expired: 'истекла' };
const PAYMENT = { succeeded: 'успешно', pending: 'в обработке', failed: 'отклонён банком', refunded: 'возвращён' };
let demo = { brands: [], accounts: [] };

// Только textContent: данные кабинета — данные, не разметка.
function el(tag, cls, text) {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
}

async function api(path, options = {}) {
  const res = await fetch(path, options);
  const body = await res.json().catch(() => ({}));
  if (!res.ok) throw Object.assign(new Error(body.error || `HTTP ${res.status}`), { status: res.status });
  return body;
}

// sessionStorage бывает недоступен (приватный режим) — тогда сессия живёт до перезагрузки страницы.
const session = {
  get() { try { return sessionStorage.getItem('vetrilo-token'); } catch { return null; } },
  set(token) { try { sessionStorage.setItem('vetrilo-token', token); } catch { /* только в памяти */ } },
  clear() { try { sessionStorage.removeItem('vetrilo-token'); } catch { /* нечего чистить */ } },
};

function show(view) {
  for (const id of ['landing', 'login', 'cabinet']) $(id).hidden = id !== view;
  $('logout').hidden = view !== 'cabinet';
  $('error').hidden = true;
}

function fail(e) {
  $('error').textContent = 'Ошибка: ' + e.message;
  $('error').hidden = false;
}

// Цвет витрины доходит и до виджета: --emi-accent наследуется внутрь его Shadow DOM.
function theme(brand) {
  const color = brand ? brand.color : '#287b57';
  document.documentElement.style.setProperty('--brand', color);
  document.documentElement.style.setProperty('--emi-accent', color);
  $('home').textContent = brand ? brand.name : 'Ветрило';
  document.title = `${brand ? brand.name : 'Ветрило'} — VPN-сервис (демо)`;
}

function landing() {
  theme(null);
  show('landing');
  $('brands').textContent = '';
  for (const b of demo.brands) {
    const card = el('button', 'brand-card');
    card.type = 'button';
    card.style.setProperty('--brand', b.color);
    card.append(el('strong', '', b.name), el('span', 'muted', b.domain), el('span', 'muted', b.countries.join(' · ')));
    card.addEventListener('click', () => login(b));
    $('brands').append(card);
  }
}

function login(brand) {
  theme(brand);
  show('login');
  $('login-title').textContent = `Вход — ${brand.name}`;
  $('accounts').textContent = '';
  for (const a of demo.accounts.filter((x) => x.brand === brand.id)) {
    const item = el('button', 'account');
    item.type = 'button';
    item.append(el('strong', '', a.name), el('span', 'muted', a.situation));
    item.addEventListener('click', async () => {
      try {
        const { token } = await api('demo-api/v1/login', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ account_id: a.id }),
        });
        session.set(token);
        await cabinet(token);
      } catch (e) {
        fail(e);
      }
    });
    $('accounts').append(item);
  }
}

function row(dl, term, value) {
  dl.append(el('dt', '', term), el('dd', '', value));
}

async function cabinet(token) {
  let c;
  try {
    c = await api('demo-api/v1/customer', { headers: { Authorization: 'Bearer ' + token } });
  } catch (e) {
    // Токен не узнан (после перезапуска сервера сессии сброшены) — обратно на витрины; при сбое сервера сессию не стираем.
    if (e.status === 401) {
      session.clear();
      return landing();
    }
    return fail(e);
  }
  theme(demo.brands.find((b) => b.id === c.brand.id));
  show('cabinet');
  $('hello').textContent = `Личный кабинет · ${c.name}`;
  const sub = $('subscription');
  sub.textContent = '';
  row(sub, 'Тариф', c.plan ? `${c.plan.name} — ${c.plan.price}` : 'не выбран');
  row(sub, 'Статус', STATUS[c.subscription.status] || c.subscription.status);
  row(sub, c.subscription.status === 'expired' ? 'Закончилась' : 'Действует до', c.subscription.until);
  row(sub, 'Автоплатёж', c.subscription.autorenew ? 'включён' : 'выключен');
  row(sub, 'Пробный период', c.trial_used ? 'использован' : 'доступен');
  $('countries').textContent = '';
  for (const country of c.server_countries || []) $('countries').append(el('li', '', country));
  const table = $('payments');
  table.textContent = '';
  if (!c.payments.length) table.append(el('caption', 'muted', 'Платежей нет'));
  for (const p of [...c.payments].reverse()) {
    const tr = el('tr', p.status);
    tr.append(el('td', '', p.date), el('td', '', p.amount), el('td', '', PAYMENT[p.status] || p.status), el('td', 'muted', p.note || p.method));
    table.append(tr);
  }
  // Виджет — одна строка встраивания: тег с токеном кабинета. Новый вход — новый виджет с пустым чатом.
  document.querySelector('emi-support')?.remove();
  const widget = document.createElement('emi-support');
  widget.setAttribute('token', token);
  widget.setAttribute('api', './');
  document.body.append(widget);
}

$('help').addEventListener('click', () => document.querySelector('emi-support')?.open());
$('back').addEventListener('click', landing);
$('logout').addEventListener('click', () => {
  session.clear();
  document.querySelector('emi-support')?.remove();
  landing();
});

(async () => {
  try {
    demo = await api('demo-api/v1/accounts');
  } catch (e) {
    show('landing');
    return fail(e);
  }
  const token = session.get();
  if (token) await cabinet(token);
  else landing();
})();
