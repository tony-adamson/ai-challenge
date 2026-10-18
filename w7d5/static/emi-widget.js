// Виджет поддержки Эми — встраивается в любой фронт одной строкой: <script src="emi-widget.js"></script>
// и тег <emi-support token="…" api="…">. token — токен сессии личного кабинета: по нему Эми сама спрашивает
// у бэкенда кабинета, кто пишет (браузеру о клиенте не верит). api — базовый URL Эми, по умолчанию "./"
// относительно страницы. Shadow DOM: стили сайта не ломают виджет, а стили виджета — сайт.
// Цвет — CSS-переменная --emi-accent на странице. Методы open()/close() — для своей кнопки «Нужна помощь?».
(() => {
  const CSS = `
:host{--accent:var(--emi-accent,#287b57);font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;font-size:14px;color:#1f2a27}
*{box-sizing:border-box}
button,textarea{font:inherit}
.bubble{position:fixed;right:20px;bottom:20px;width:56px;height:56px;border-radius:50%;border:0;background:var(--accent);color:#fff;font-size:24px;cursor:pointer;box-shadow:0 6px 18px rgba(0,0,0,.2);z-index:2147483000}
.panel{position:fixed;right:20px;bottom:20px;width:min(380px,calc(100vw - 32px));height:min(560px,calc(100dvh - 40px));display:flex;flex-direction:column;background:#fff;border-radius:14px;box-shadow:0 10px 32px rgba(0,0,0,.22);overflow:hidden;z-index:2147483000}
.panel[hidden],.bubble[hidden]{display:none}
header{display:flex;align-items:center;gap:8px;padding:12px 14px;background:var(--accent);color:#fff}
header span{flex:1;font-size:12px;opacity:.85}
.close{background:transparent;border:0;color:#fff;font-size:22px;line-height:1;cursor:pointer}
.messages{flex:1;overflow-y:auto;padding:12px;display:flex;flex-direction:column;gap:10px;background:#f7f8f5}
.msg{max-width:88%;padding:9px 12px;border-radius:12px;line-height:1.5;white-space:pre-wrap;overflow-wrap:anywhere}
.me{align-self:flex-end;background:var(--accent);color:#fff}
.emi{align-self:flex-start;background:#fff;border:1px solid #e3e8e4}
.error{color:#a3472b}
.dots{display:inline-flex;gap:4px;padding:4px 0}
.dots i{width:7px;height:7px;border-radius:50%;background:var(--accent);animation:emi-bounce 1.2s infinite ease-in-out}
.dots i:nth-child(2){animation-delay:.15s}
.dots i:nth-child(3){animation-delay:.3s}
@keyframes emi-bounce{0%,80%,100%{transform:translateY(0);opacity:.35}40%{transform:translateY(-5px);opacity:1}}
.stage{display:block;margin-top:2px;font-size:12px;color:#5d6b66}
@media (prefers-reduced-motion:reduce){.dots i{animation:none;opacity:.7}}
.handed{margin-top:8px;padding:6px 9px;border-radius:8px;background:#fff4dc;color:#7a5a12;font-size:12px}
details{margin-top:8px;font-size:12px;color:#5d6b66}
summary{cursor:pointer}
.quote{margin-top:6px;padding-left:8px;border-left:3px solid var(--accent)}
.quote b{display:block;font-weight:600;color:var(--accent)}
form{display:flex;gap:6px;padding:10px;border-top:1px solid #e3e8e4}
textarea{flex:1;resize:none;border:1px solid #d5ddd8;border-radius:10px;padding:8px 10px;outline:0}
form button{border:0;border-radius:10px;padding:0 14px;background:var(--accent);color:#fff;cursor:pointer}
form button:disabled{opacity:.5;cursor:not-allowed}`;

  function el(tag, cls, text) {
    const node = document.createElement(tag);
    if (cls) node.className = cls;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  class EmiSupport extends HTMLElement {
    connectedCallback() {
      if (this.shadowRoot) return;
      const root = this.attachShadow({ mode: 'open' });
      // Разметка — статичный шаблон; всё, что приходит от сервера, вставляется только через textContent.
      root.innerHTML = `<style>${CSS}</style>
<button class="bubble" type="button" aria-label="Чат поддержки">?</button>
<section class="panel" hidden>
  <header><b>Эми</b><span>поддержка · ответ по базе знаний</span><button class="close" type="button" aria-label="Закрыть">×</button></header>
  <div class="messages"></div>
  <form><textarea rows="2" maxlength="1000" placeholder="Опишите вопрос"></textarea><button type="submit">➤</button></form>
</section>`;
      this.q = (selector) => root.querySelector(selector);
      this.q('.bubble').addEventListener('click', () => this.open());
      this.q('.close').addEventListener('click', () => this.close());
      this.q('form').addEventListener('submit', (event) => {
        event.preventDefault();
        this.send();
      });
      this.q('textarea').addEventListener('keydown', (event) => {
        if (event.key === 'Enter' && !event.shiftKey) {
          event.preventDefault();
          this.send();
        }
      });
      this.say('emi', 'Здравствуйте! Я Эми, ассистент поддержки. Я вижу ваш кабинет — подписку и платежи. Чем помочь?');
    }

    open() {
      if (!this.shadowRoot) return;
      this.q('.panel').hidden = false;
      this.q('.bubble').hidden = true;
      this.q('textarea').focus();
    }

    close() {
      if (!this.shadowRoot) return;
      this.q('.panel').hidden = true;
      this.q('.bubble').hidden = false;
    }

    say(cls, text) {
      const box = el('div', 'msg ' + cls, text);
      this.q('.messages').append(box);
      box.scrollIntoView({ block: 'end' });
      return box;
    }

    // Пока ждём ответ: анимация и счётчик секунд показывают, что работа идёт. Этапы — по таймеру, примерно
    // в том порядке, в каком их проходит сервер (кабинет → база знаний → проверка цитат): потока событий у API нет.
    waiting(box) {
      const STAGES = [[0, 'Читаю вопрос'], [2, 'Смотрю ваш кабинет и подписку'], [5, 'Ищу ответ в базе знаний'], [12, 'Сверяю цитаты с источниками'], [25, 'Готовлю ответ — ещё немного']];
      const dots = el('span', 'dots');
      dots.append(el('i'), el('i'), el('i'));
      const stage = el('span', 'stage');
      box.append(dots, stage);
      const started = Date.now();
      const tick = () => {
        const sec = Math.floor((Date.now() - started) / 1000);
        stage.textContent = `${STAGES.filter(([at]) => sec >= at).pop()[1]}… ${sec} с`;
      };
      tick();
      const timer = setInterval(tick, 1000);
      return () => clearInterval(timer);
    }

    async send() {
      const input = this.q('textarea');
      const button = this.q('form button');
      const text = input.value.trim();
      // Пока идёт ответ, второй вопрос не шлём: Enter обходит disabled у кнопки.
      if (!text || button.disabled) return;
      input.value = '';
      this.say('me', text);
      const box = this.say('emi pending', '');
      const stop = this.waiting(box);
      button.disabled = true;
      let reply;
      try {
        const base = new URL(this.getAttribute('api') || './', document.baseURI);
        const res = await fetch(new URL('api/widget/chat', base), {
          method: 'POST',
          headers: { 'Content-Type': 'application/json', Authorization: 'Bearer ' + (this.getAttribute('token') || '') },
          body: JSON.stringify({ text }),
        });
        reply = await res.json().catch(() => ({ kind: 'error', text: `Поддержка ответила HTTP ${res.status}` }));
      } catch (e) {
        reply = { kind: 'error', text: 'Нет связи с поддержкой: ' + e.message };
      } finally {
        stop();
        button.disabled = false;
        input.focus();
      }
      this.render(box, reply);
    }

    render(box, reply) {
      box.className = 'msg emi' + (reply.kind === 'error' ? ' error' : '');
      box.textContent = reply.kind === 'answer' ? reply.answer : reply.text || 'Что-то пошло не так.';
      // Плашка — по факту записи в CRM от сервера, а не по словам модели.
      // Сервер сам вклеивает запись о тикете в текст, когда модели нечего было процитировать — второй раз её не показываем.
      const handed = (reply.handed || []).filter((h) => !box.textContent.includes(h));
      if (handed.length) box.append(el('div', 'handed', 'Передано специалисту. ' + handed.join(' ')));
      if (reply.quotes && reply.quotes.length) {
        const sources = el('details');
        sources.append(el('summary', '', `Источники (${reply.quotes.length}) — цитаты проверены`));
        for (const q of reply.quotes) {
          const item = el('div', 'quote');
          item.append(el('b', '', q.label), document.createTextNode('«' + q.text + '»'));
          sources.append(item);
        }
        box.append(sources);
      }
      box.scrollIntoView({ block: 'end' });
    }
  }

  if (!customElements.get('emi-support')) customElements.define('emi-support', EmiSupport);
})();
