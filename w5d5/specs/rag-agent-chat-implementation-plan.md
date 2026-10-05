# План реализации: чат с RAG, памятью и созданием агента

# Блок 1. Для человека

## 1. Кратко

В `w5d5` появится чат, работающий в браузере и в Electron на macOS. Каждый вопрос запускает поиск; ответы показывают источники. История и цель/уточнения/ограничения/термины сохраняются. После обсуждения чат создаёт законченный read-only Claude Code-агент и проверяет его на предметных файлах.

## 2. Что будет сделано

Один локальный Node API, общий небольшой HTML/CSS/JS-интерфейс, Electron-окно и Python-helper для имеющейся книжной FTS5. Прежние API и research/DeepWiki MCP используются по ограниченным контрактам. Реализация последовательно проходит десять этапов; итог принимается по двум живым диалогам по 12–15 пользовательских ходов.

## 3. Что не делаем

Telegram, глобальную установку агентов, Linear/PR pipeline, OCR/новую vector DB, scheduler, cloud deployment и публикацию `.app`. Прежние дни, книги и `g-harness` не изменяются. Видео записывается пользователем по готовому сценарию; код и инструкция запуска входят в результат.

## 4. Риски

- РИСК-1: лексический поиск может пропустить полезный материал. Проверяем релевантность и отказ без подтверждения.
- РИСК-2: память может потерять раннее ограничение. Проверяем ошибку ответа, продолжение и перезапуск.
- РИСК-3: успешный subprocess не доказывает качество агента. Проверяем критерии на независимых fixtures и hashes конкретного комплекта.

## 5. Как проверить

После реализации: `npm --prefix w5d5 run check`, `npm --prefix w5d5 run package` и `npm --prefix w5d5 run eval:live`. Дополнительно реальный запуск окна и того же чата в браузере. Нет доступного API/MCP/Claude — соответствующий live gate красный, результат не объявляется завершённым.

## 6. Готовность

BUILD_IN_PROGRESS — Plan Challenger и Lean Plan Challenger: APPROVE. Согласованный дизайн: `w5d5/SOLUTION.md`, READY_FOR_PLANF3. План не является отчётом о работающем приложении.

---

# Блок 2. Для агента

## 1. Метаданные

Корень команд: `/Users/admin/MyProjects/ai_challenge`. Фокус: `w5d5`. Дата: 2026-10-03; baseline ветка `w5d4`, HEAD `41fc8e7`. Дизайн: `w5d5/SOLUTION.md`; evidence: `w5d5/CURRENT_STATE.md`; интервью: `w5d5/GRILL.md`. Все фазы первоначально `[]`; статусы исполнения `[]`, `[wip]`, `[x]`, `[f]`. Не создавать ветку/commit/push в рамках плана без отдельного поручения.

До правок прочитать текущие инструкции и `/Users/admin/.codex/skills/scope-review/references/test-rules.md`, выполнить `git status --short`. Сохранить посторонние untracked `SESSION_STATE.md` и `docs/transcript_lektsii_po_ii.pdf`. Shell-команды в исполнении оборачивать через `rtk`; команды ниже — запускаемые payload-команды, без изменения смыслов.

## 2. Приоритет источников

Требования пользователя → текущие контракты референса → согласованный SOLUTION → этот план → локальный стиль. Конфликт приводит к остановке. SOLUTION разделы 5–12 обязательны целиком, включая лимиты и негативные исходы; краткое перечисление ниже не отменяет их.

## 3. Авторитетные требования

REQ-1 история; REQ-2 локальный RAG каждый вопрос; REQ-3 подтверждённые источники; REQ-4 task memory; REQ-5 два длинных сценария/код/видео; REQ-6 общий browser/Electron; REQ-7 законченный агент; REQ-8 предметная Claude-проверка; REQ-9 прежний API; REQ-10 книги/g-harness/research/DeepWiki; CON-1 независимая папка. Определения и происхождение — SOLUTION §2.

## 4. Контракт минимальности

| Категория | Бюджет | Обоснование |
|---|---|---|
| Прямые зависимости | MCP client 2.3.0; dev Electron 44.5.1, TypeScript 7.0.2, @types/node 25.9.9, @electron/packager 20.3.0 | REQ-6/10 и строгая сборка; точные версии, npm lockfile |
| Persistent state | JSON чатов, immutable комплекты и их отчёты; служебный lockfile | REQ-1/4/7/8; нет новой persistent queue/index |
| Runtime подсистемы | Один local API, Electron shell, конечные Python/Claude процессы | Браузерный режим и существующая FTS5; без службы/worker |
| Абстракции | Только функции/типы предметных модулей | Нет generic framework, plugin system, export factory |
| Документы | README, сценарий видео, отчёт приёмки и план | Использование/проверка результата |

Полный перенос исследователя и React/Express отклонены дизайном. Нельзя добавлять зависимости ради тестового framework. Моки допустимы только на внешних API/MCP; свои файлы, FTS5, HTTP и subprocess проверяются реально. Тестовые ожидания задаются по независимым inputs, не по выходу той же реализации.

## 5. Бюджет файлов

| Новые файлы | Назначение | Требования | Можно избежать? |
|---|---|---|---|
| `src/server.ts`, `src/store.ts`, `src/web.ts` | HTTP/session/lifecycle, snapshot, browser entry | 1/4/6 | Нет; один server для обоих режимов |
| `src/memory.ts`, `src/chat.ts` | Evidence patches и последовательность хода | 2/3/4 | Нет; без generic orchestration |
| `src/rag.ts`, `scripts/search_kb.py` | Bounded retrieval, FTS5 в поддерживаемом Python | 2/10 | Нет; внешняя DB неизменна |
| `src/clients.ts` | Прежние LLM wire requests и MCP allowlist | 9/10 | Нет; без собственной реализации MCP |
| `src/artifacts.ts`, `src/claude.ts` | Bundle/spec hashes и read-only subprocess | 7/8 | Нет; ответственность предметная |
| `src/main.ts` | Electron lifecycle/navigation | 6 | Нет privileged preload |
| `static/index.html`, `static/app.js`, `static/style.css` | Общий интерфейс | 1–10 | Нет отдельного desktop UI |
| `tests/server.test.ts`, `tests/store.test.ts`, `tests/memory.test.ts`, `tests/rag.test.ts`, `tests/clients.test.ts`, `tests/chat.test.ts`, `tests/artifacts.test.ts`, `tests/claude.test.ts`, `tests/search_kb_test.py` | Поведенческие контракты | 1–10 | Нужны независимые проверки ошибок |
| `scripts/eval.ts`, `tests/scenarios/docs.json`, `tests/scenarios/review.json` | Два заранее заданных ledger и live driver | 5 | Без eval framework |
| `package.json`, `package-lock.json`, `tsconfig.json`, `.node-version`, `.python-version`, `.gitignore` | Build/точные runtime/локальные outputs | 6 | Стандартный tooling |
| `README.md`, `VIDEO.md`, `ACCEPTANCE.md` | Запуск/запись/реальные результаты | 5 | Без дополнительных документов |

Все пути в таблице относительны `w5d5/`. Оценка: 14 production-файлов, 12 test/eval/fixture файлов, 6 конфигов и 3 новых документа, всего 35 новых файлов; существующие 4 дизайн/план-документа можно уточнять только при реальном amendment. До 1700 production LOC +650 tests/eval/fixtures LOC +100 config LOC. Generated lockfile и документация не считаются source LOC, но учитываются в количестве файлов.

**Estimated LOC net: ~2450.** Стоп при >2× количества файлов либо LOC; до продолжения показать stat и причину. У untracked дополнительно считать файлы/строки внутри allowlist, не stage ради статистики.

Runtime preconditions (выполнить до build; будущие pins создаются в фазе 1, значения ниже уже зафиксированы для этого локального приложения):
- Node ==25.8.1, `.node-version`: `node -e "if(process.version !== 'v25.8.1')process.exit(1)"`.
- Python ==3.14.5, `.python-version`, FTS5 и реальная книжная схема: `/opt/homebrew/bin/python3 -c 'import sys,sqlite3; assert sys.version_info[:3] == (3,14,5); c=sqlite3.connect("file:/Users/admin/MyProjects/books-rag/db/books.sqlite3?mode=ro",uri=True); assert c.execute("select count(*) from chunks where chunks match ?",("agent",)).fetchone()[0]>0'`.
- npm ==11.14.1: `node -e "const c=require('node:child_process');if(c.execFileSync('npm',['--version'],{encoding:'utf8'}).trim()!=='11.14.1')process.exit(1)"`.
- Claude ==2.1.288 и нужные restricted flags: `node -e "const c=require('node:child_process');const v=c.execFileSync('/Users/admin/.local/bin/claude',['--version'],{encoding:'utf8',timeout:10000});if(!v.includes('2.1.288'))process.exit(1);const h=c.execFileSync('/Users/admin/.local/bin/claude',['--help'],{encoding:'utf8',timeout:10000});for(const f of ['--restricted','--strict-mcp-config','--permission-mode','--agents','--no-session-persistence'])if(!h.includes(f))process.exit(1)"`.

API/Claude auth и текущий MCP endpoint до live фазы UNKNOWN; закрываются bounded живыми вызовами в фазе 10, а не чтением имени env-переменной. npm installation — phase 1, network timeout 30s/retries 1 на fetch, whole-process 180s, без global install. Если эти лимиты недостаточны — сообщить ошибку; не повторять без нового основания.

## 6. Трассировка требований

| Контракт | Фазы | Наблюдаемая приёмка |
|---|---|---|
| REQ-1 | 1/5/8/10 | Полная история/settings после закрытия и повторного открытия |
| REQ-2/3 | 3/5/8/10 | Retrieval каждого turn; source IDs/quotes реально принадлежат ему; honest empty-search |
| REQ-4 | 2/5/8/10 | Evidence только user; ранняя цель/constraints после ошибок/restart |
| REQ-5 | 10 | Два ledger, 12–15 ходов каждый, отчёт + VIDEO |
| REQ-6 | 1/8/9 | `.app` и браузер реально открывают один store и интерфейс |
| REQ-7 | 6/8/10 | Конкретный комплект без placeholders; изменение task делает PASS stale |
| REQ-8 | 7/8/10 | Именно agent file запускается в restricted Claude; report не сводится к exit0 |
| REQ-9 | 4/8/10 | Provider-specific request и явно выбранная модель |
| REQ-10 | 3/4/10 | Books/doc citations и фактический research/DeepWiki trace |
| CON-1 | Все | Изменения только allowlist, источники неизменны |

## 7. Семантика операций

HTTP и полные условия — SOLUTION §5/10. POST session исключение из авторизации; остальные endpoints защищены cookie, Host/Origin/cross-site policy. Body JSON ограничен 64KiB. Mutations ask/retry/build/validate получают chatId из GET chats; validate получает artifactId из GET chat; cancel получает operationId из ответа accepted/GET progress. Unknown IDs 404, bad input400, auth401, origin403, busy409. OperationIds в памяти; один active action на чат. Повтор cancel terminal — no-op с конечным статусом. Retry продолжает тот же user turn и не дублирует сообщение. Нет автоматического retry сетевых failures.

Source/file endpoints JSON; download только attachment/octet-stream/nosniff. Frontend обслуживается по фиксированному allowlist. Пути от модели не принимаются. Build создаёт новую immutable version, не затирает старую. Validate пишет отчёт отдельно; unavailable/fail/stale не означают ready. Cancel abort-ит собственные I/O/children и оставляет ранее применённую память.

Новые контракты имеют только новых consumers: `static/app.js`, `src/main.ts`, `scripts/eval.ts`, tests. Старые enum/поля/env/API не переименовываются. Референсы: `w4d5/src/agent.rs:271` (settings), `w4d5/src/agent.rs:864` (wire), `w4d5/src/mcp.rs:34` (endpoints), `w4d5/src/watch.rs:458` (search persistence). Книжный schema расположен во внешнем `/Users/admin/MyProjects/books-rag/scripts/books_kb.py:24`. Приложение не предполагает, что GitHub metadata доказывает содержимое кода.

## 8. Lifecycle состояния

Store: schemaVersion/chatId/settings/messages.content/task/artifact references; atomic tmp+rename. Корректность schema проверяется до записи/после load; неизвестная/повреждённая версия видна, без перезаписи исходника. Чтение failure не превращается в пустой chat. Lockfile/PID исключает второго writer и снимается только своим экземпляром; stale cleanup по SOLUTION.

User save memory_pending → evidence-validated memory commit + pending → accepted либо response failure/interrupted. Extraction failure → memory_failed блокирует следующий ход/build до успешного retry. Restart memory_pending→memory_failed; pending→interrupted, без нового API вызова. Evidence сохранённой памяти не зависит от успеха ответа. Полная история сохраняется, prompt берёт последние 12 messages. MemoryEntry и canonical specHash — SOLUTION §7/8; implement буквально, не hash всего JSON.

Snapshots — источник истины диалога. Exact sent snippets — источник истины citations. Immutable bundle bytes +specHash+report — источник ready. Отчёт существует в одном экземпляре artifact-directory; snapshot содержит reference. Удаления UI/миграции старых чатов нет. Temporary fixtures удаляются только в own temp root; отчёты сохраняются. Source roots и outputs раздельны. Sessions/capabilities/operations только RAM; shutdown инвалидирует их и завершает только свои children.

## 9. Фазы реализации

Каждая фаза последовательная, без parallel builder. Перед тестами прочитать test-rules; red на независимых тестах до production, затем green. Allowlist включает только перечисленные файлы; другие нужны → needsFiles/остановка. Общие запреты §4/11 действуют во всех фазах. Команды из repo root; phase scripts определены в package.json, выполняют strict build и указанные реальные tests, не grep реализации.

### [x] Фаза 1. Runtime, HTTP и сохраняемый чат

Allowlist: package/config/pins/gitignore, src/server.ts/store.ts/web.ts, tests/server.test.ts/store.test.ts. Создать npm build/check scripts для будущих фаз (tests несуществующей фазы до её завершения не включать в общий check). Node built-in test runner, TypeScript strict/noUncheckedIndexedAccess, frontend checkJs. Режим web запускает один loopback server и штатный browser; app data root явный, `.env` читается без shell sourcing. Реализовать capability/cookie, source/file безопасную выдачу, body bound, IDs, in-memory operations/cancel plumbing, thin handlers, lockfile/shutdown и atomic snapshot. Заглушки model actions не считаются готовностью — подключатся фазой 5. API operations тестируются с коротким реальным cancellable child fixture; это не production mock filesystem.

**Тесты до реализации**: `w5d5/tests/server.test.ts` `w5d5/tests/store.test.ts`.
**Команда проверки**: `npm --prefix w5d5 run check:phase1`.
Exit0: actual HTTP 401/403/404/409, bootstrap one-use/expiry, no raw HTML/JS fixture execution, byte-preserving save/reopen, malformed JSON не перезаписан, second writer rejected, cancel child остановлен, повтор cancel no-op. Setup own temp dirs/free ports; cleanup listener/child/own dirs in finally. Verifier: cookie не попадает в logs; current server не убивает чужой PID.
**Estimated LOC net: ~390** (250 production +55 tests +85 config).

### [x] Фаза 2. Evidence-память

Allowlist: src/memory.ts, tests/memory.test.ts. Typed patch schema, keyed upsert/remove, goal, terms/constraints/clarifications. Exact user message evidence; новые/заменённые entries подтверждаются текущим сообщением. Не принимать assistant/source evidence. Нормализация и отсутствие duplicate keys; remove отсутствующего key — ошибка. Extraction как API call подключится фазой 5.

**Тесты до реализации**: `w5d5/tests/memory.test.ts`.
**Команда проверки**: `npm --prefix w5d5 run check:phase2`.
Exit0: заранее заданные msgIDs/quotes меняют только ожидаемые keys; forged evidence отклонено; замена оставляет один current value. Verifier: нет модели как единственного oracle.
**Estimated LOC net: ~230** (170 +60 tests).

### [x] Фаза 3. Локальный RAG

Allowlist: src/rag.ts, scripts/search_kb.py, tests/rag.test.ts/search_kb_test.py. Helper stdlib: read-only FTS5 books AND→OR/BM25; transient doc FTS allowlist, symlink escape denied. Python executable явный. Quotas книги4/документы4, max12 snippets/20kchars, file2MiB/500docs, output1MiB, timeout15s/retry0. Source IDs source+chunk_no; nullable pages, docs path/lines; exact sent text сохраняется. Не копировать индекс.

**Тесты до реализации**: `w5d5/tests/rag.test.ts` `w5d5/tests/search_kb_test.py`.
**Команда проверки**: `npm --prefix w5d5 run check:phase3`.
Exit0: actual small FTS5 fixture с заранее заданными нужными IDs, AND→OR fallback, отсутствующие EPUB pages не выдуманы; hidden/.env/symlink outside excluded; helper timeout/output bound visible. Setup temporary corpus/DB, cleanup в finally. Verifier: DB opened mode=ro и unchanged bytes.
**Estimated LOC net: ~250** (160 +90 tests).

### [x] Фаза 4. API и MCP-клиенты

Allowlist: src/clients.ts, tests/clients.test.ts. Settings Cerebras/DeepSeek/OpenRouter, key_env и provider-specific semantics референса. Явный model выбор, stream:false/structured response, никакого silent fallback. MCP SDK connect/list/call с research.search_repositories/deepwiki.ask_wiki_question; ≤4 calls/turn. Нормализовать provenance, не обращаться к notify/watch/write. API60s/MCP20s, retries0; закрывать client и transport по finally/cancel. Не дублировать полный MCP output в trace.

**Тесты до реализации**: `w5d5/tests/clients.test.ts`.
**Команда проверки**: `npm --prefix w5d5 run check:phase4`.
Exit0: внешний bounded mock endpoint наблюдает ожидаемые независимо заданные request fields, не получает ключи в payload/log; forbidden tool отклонён до вызова; timeout/networkfail без model switching. Verifier: реальные SDK методы и старый wire, не mock самого production adapter.
**Estimated LOC net: ~240** (190 +50 tests).

### [x] Фаза 5. Координатор нового вопроса

Allowlist: src/chat.ts, src/server.ts, src/store.ts, src/web.ts, src/clients.ts, tests/chat.test.ts. Встроить phase2/3/4, операции ask/retry. Сохранить память ДО retrieval/ответа. Каждый user turn делает local RAG; external optional. Structured answer/source IDs/literal quotes ≥20 chars whitespace-normalized; schema/citation repair ≤1, API/network retry0. Empty/unavailable support приводит к видимому отказу, а не свободному ответу. Poll progress по operationId. Нельзя принимать partial output после cancel.

**Тесты до реализации**: `w5d5/tests/chat.test.ts`.
**Команда проверки**: `npm --prefix w5d5 run check:phase5`.
Exit0: новый constraint→response fail→следующий ход→reopen сохраняет constraint; extraction fail блокирует next/build; pending restart статусы; retry без duplicate; каждый короткий followup имеет retrieval; forged/mismatched source/quote отклоняется. Свои store/RAG real, mock только внешний LLM. Verifier: saved source = exactly sent snippet, tail12 не подменяет fullhistory.
**Estimated LOC net: ~220** (160 +60 tests).

### [x] Фаза 6. Законченный комплект

Allowlist: src/artifacts.ts, src/chat.ts, src/server.ts, tests/artifacts.test.ts, tests/chat.test.ts (подтверждение критериев и BUILD). Build на current goal+task и знании, no unresolved conflict/emptygoal/memory_failed. Backend owns artifact paths/name. `.claude/agents/name.md` с subset Read/Grep/Glob, model:inherit, README, конкретные fixtures/criteria. Без TODO/заглушек/hooks/MCP/writable memory. Canonical specHash fixed format+sorted current values, bytes file hashes, неизменяемые versions. Независимые критерии согласованы до build, не генерируются по ответу агента.

**Тесты до реализации**: `w5d5/tests/artifacts.test.ts`.
**Команда проверки**: `npm --prefix w5d5 run check:phase6`.
Exit0: name traversal/forbidden capabilities rejected, старый bundle не перезаписан, taskvalue change makes stale/evidence-only does not, alteredbytes makes notready, no missingreport=>PASS. Verifier: output outside roots невозможен.
**Estimated LOC net: ~250** (200 +50 tests).

### [x] Фаза 7. Проверка через Claude

Allowlist: src/claude.ts, src/artifacts.ts, src/server.ts, src/chat.ts (подключение validate), tests/claude.test.ts. Запуск именно saved bundle через --agents/--agent, restricted, Read/Grep/Glob, dontAsk, strict empty MCP, disableAllHooks, no-session-persistence. Без --bare. Managed hooks preflight; неподдающиеся отключению→unavailable. Actual run ≤3/bundle, each180s, concurrently drain stdout/stderr1MiB, abort/kill on timeout/cancel. Fixture hashes before/after, JSON/tool trace parsing и предметные критерии; exit0 не равенPASS. Manual semantic unresolved→notready.

**Тесты до реализации**: `w5d5/tests/claude.test.ts`.
**Команда проверки**: `npm --prefix w5d5 run check:phase7`.
Exit0: реальные fixture executables (process boundary) проверяют argv, largeoutput, timeout/cancel/noorphan, exit0 malformedresult rejected; запись fixture выявлена. Это offline boundary test, liveClaude только phase10. Verifier: no capability escalation/no auth fallback; report binds spec/files.
**Estimated LOC net: ~195** (150 +45 tests).

### [x] Фаза 8. Общий веб-интерфейс

Allowlist: static/index.html/app.js/style.css, src/server.ts, ACCEPTANCE.md (только реальные результаты browser walkthrough). Список чатов, provider/model controls, разговор, source details, task panel, build/validate/cancel/results. Plain textContent, no inline/remote scripts. Bootstrap fragment read+clear, cookie API; errors/progress/disconnect видны. Read APIs дают IDs, UI не придумывает пути. Нет отдельного state store в renderer.

**Команда проверки**: `npm --prefix w5d5 run check:phase8`.
Exit0: strict/checkJs build + HTTP/auth tests phase1/5/6/7; затем обязательный реальный browser walkthrough: create/settings/question/source/task/build/cancel/reopen. UI acceptance фиксировать в ACCEPTANCE, screenshot допустим, синтетический DOM oracle не добавлять. Verifier вручную проверяет видимость отсутствующего источника и disabled actions.
**Estimated LOC net: ~300** (production).

### [x] Фаза 9. Electron и локальная упаковка

Allowlist: src/main.ts, package.json, README.md, ACCEPTANCE.md (только реальные результаты package/window/browser проверки). Один BrowserWindow sandbox/contextIsolation/noNode/noPreload, sameHTTP origin, denied external nav/newwindow, checked http(s) opens external browser. Собственный server shutdown, shared root и second-instance lock. `npm run desktop` и `npm run web` запускают уже built app, `npm run package` компилирует/упаковывает локальную .app через pinned packager; include static/Python helper, outputs ignored. Нет notarization/deploy.

**Команда проверки**: `npm --prefix w5d5 run package`.
Exit0: executable .app bundle создан; обязательный ручной запуск packaged app и browser-mode безElectron, один chatId после restart каждого, sourceopen и cancel. Проверить helper path внутри bundle, Node vs Electron embedded runtime capability. Verifier: реальные window/process observations; packaging само не доказываетUI.
**Estimated LOC net: ~135** (120 production +15 config).

### [wip] Фаза 10. Две живые приёмки и handoff

Allowlist: scripts/eval.ts, tests/scenarios/docs.json/review.json, package.json, README.md/VIDEO.md/ACCEPTANCE.md. Сначала написать два независимых ledgers по 12–15 userturns. Docs: read-only документационный аналитик; review: read-only reviewer ограниченного codefixture. Каждый содержит earlygoal/2–3constraints/term/explicitchange/короткий followup/restart/build/validate; absentfact и data promptinjection. Driver идёт через защищённый HTTP API с тем же coordinator, переоткрывает реальный сервер/store на середине, проверяет все turns, sources и task противledger. Fixture expectations заранее authored, не из generated output. API60/MCP20/helper15/Claude180sec; whole driver ≤3600sec, retries0 кроме предусмотренного единственного citationrepair. Отчёт связывает exactbundle/spec/criteria; semantic сомнения требуют ручной сверки и неPASS. Положительный API/MCP/Claude preflight входит в те же runs, никаких лишних cloudwrite.

**Тесты до реализации**: `w5d5/tests/scenarios/docs.json` `w5d5/tests/scenarios/review.json` (независимые inputs/expected state; driver не frozen test).
**Команда проверки**: `npm --prefix w5d5 run eval:live`.
Exit0 ТОЛЬКО: оба ledger прошли, каждый question retrieval, валидные relevant citations, цель/earlyconstraints сохранены послеrestart, оба финальныхагента прошли предметную проверку, live research+DeepWiki evidence есть хотя бы в соответствующих repo prompts. Нет auth/connection/semantic confirmation→exit1 и честный отчёт. Reports в ignored local data, ACCEPTANCE только sanitised observations/no secrets. Temporary dirs cleanup послеrun, keep result/report. VIDEO даёт маршрут записи: browser chat→RAG→task→restart→bundle→actualvalidation и Electronwindow; не отмечать видео записанным без файла.
**Estimated LOC net: ~240** (100 eval+140 fixtures).

## 10. Команды проверки

Каждый `check:phaseN` — build plus named behavioral tests текущей фазы (для UI фазы related backend tests). `npm --prefix w5d5 run check`: все implemented TS tests и Python tests, strict compilation/checkJs; no external calls. `npm --prefix w5d5 run package`: конечный build/packager command. `npm --prefix w5d5 run eval:live`: bounded driver, отдельный явный network/subprocess run с чётким exitstatus.

Документы: `python3 /Users/admin/.codex/skills/planf3/scripts/check-citations.py w5d5/specs/rag-agent-chat-implementation-plan.md --root /Users/admin/MyProjects/ai_challenge` и такая же команда для `w5d5/SOLUTION.md`. Всё запускается через rtk proxy. Проверки не изменяют production/source roots.

## 11. Стоп-условия

Нужен файл вне allowlist; diff >2×; новая dependency/subsystem; изменился согласованный контракт; sources требуют secret/private corpus вне allowlist; managed hooks не отключаются; CLI/SDK capability не подтверждена; failed sameerror twice — объяснить причину до новой правки, третий раз blocker. Runtime red → не обходить новым runtime/model. Live failure не прячет offlinegreen. Архитектурная поправка → BLOCKED_FOR_SOLUTION_AMENDMENT, overbuilt → BLOCKED_BY_SCOPE_OVERDESIGN.

## 12. Политика проверки

Plan/Lean Challenger независимые read-only; принятые blocking исправляются с одним rerun. После кода отдельный code review и ops-review I/O, приёмка главным агентом. Если skill отсутствует — сообщить, не имитировать егоPASS; отдельное read-only review возможно без ложного имени навыка. Не делегировать приёмку builder. Testfailure надо исправлять в production, не ослаблять frozen criteria. Native/browser manual gate обязателен даже при всехgreen unitchecks.

## 13. Финальный отчёт

Что реализовано/как запустить; actual `git diff --stat` + untracked w5d5 file/LOC count; offline/live/UI/package outcomes с exactcommands и exitcodes; два scenario reports и readybundle paths; unresolved UNKNOWN. Что проверить: раннее constraint послеrestart, источник подтверждает claim, changedtask makesstale, cancel kills ownchild, `.app` helper работает. Допущения: Node/Python baseline CONFIRMED локальной проверкой; auth/MCP/Claude и corpusrelevance до live UNVERIFIED. Видео: подготовлен маршрут, actualrecording отдельно. Не обещать полную готовность при красном обязательномgate.

## 14. Amendments

2026-10-03: пользователь согласовал Electron+browser вместоSwiftUI. Решения API, RAG, evidence-memory, immutablebundle и restrictedverification сохранены. Реализация ещё не началась.

Readiness gate 2026-10-03: PASS — traceability, design boundary, minimality, operation/lifecycle, executable validation, command/scope. Plan Challenger: allowlist отчёта исправлен, повтор APPROVE; Lean APPROVE. Citation checks: план5/0failed, SOLUTION17/0failed. Node/Python/FTS5/npm/Claude version+flags проверены успешно. Это проверка документов и локальных runtime preconditions; npm install/build, внешний API/MCP/OAuth, оба live сценария и GUI ещё не запускались.

Фаза 1: strict build +9/9 tests PASS; independent verifier APPROVE после stale-reclaimer correction. Дополнительные реальные smoke: expiry61sec, artifact JSON/download/pathescape, active GET, shutdown-owned-child; eight-process lock contention одинwriter. Фактические source/config/tests ~509LOC против390estimate, ниже2×.

Фаза2: independent red beforeproduction; strict+7/7 PASS; freshverifierAPPROVE. Source54+tests80LOC.

Фаза3: strict+Node1/Python5 PASS; freshverifierAPPROVE послеexplicitexcludedroot regression. Живойreadonlyпоисквсуществующемкорпусе вернул4book+4doc sources. Runtimebounds естьвкоде, отдельные timeout/output smokes пока не выполнялись.

Фаза4: strict+4/4 actualupstream/MCPSDKtests PASS; freshverifierAPPROVE, source132LOC. Amendment выполнения: src/web.ts добавлен вallowlistфазы5, чтобы подключить coordinator к уже запланированному browser entry; файл уже вобщембюджете, архитектура/APIнеизменны.

Фаза5: clients.ts добавлен вallowlistдляразличения modelJSONformatfailure иtransportfailure; нужнооднократноеrepairпоSOLUTION, безповтораnetworkfail. Новыхdependencies/файловнет.

Фаза5: strict+Node24/Python5 globaltests PASS; freshverifierAPPROVE послеmalformedJSONrepairregression. Памятьcommittedbeforeответ; singleuserturnretrieval, assistantreplyTo/citations; старыйretryпосленовыхрешенийreject. ДополнительнаяопциональнаяметадатаreplyTo/citations вновомформатеописанавSOLUTION, migrationнепотребовалась.

Фаза6: критерии подтверждаются отдельным пользовательским сообщением с JSON Verification; deterministic evidence patch, локальный retrieval обязателен. BUILD не получает fixture contents/expected/forbidden и историю подтверждения. Критерии привязаны specHash и file hashes. Независимый RED integration test написан до реализации. Реальный Cerebras qwen-3.8-27b JSON preflight PASS; секреты не выводились.

Фаза6: root artifacts3/3+chat4/4 PASS; freshverifierAPPROVE, rollback иsymlinkescape дополнительно подтверждены. Phase7REDmissingclaude.ts ожидаемый. Allowlistphase7 включает chat.ts дляреальногоподключенияvalidate, ужезапланированныйфайлбезновогослоя. Политикиmacmanaged knownpaths отсутствуют; CLIauth покаloggedIn:false.

Фаза7: strict/offline8/8 PASS; freshverifierAPPROVE послеindependent deniedRead/missingreceipt RED иисправленияуспешногоtool_result. MandatoryReadtrace требуетreceiptпоID; requestодиннетдоказательство. Rootglobalдоregression34Node+5PythonPASS. DefaultCLI knownabsoluteinstalledpath дляFinder. Authfallbackисключён; liveClaudeнепройденбезвхода.

Phase8 livebrowser выявил citationnumber mismatch послеодногоремонтногоответа: валидаторнеослабляем. Executionamendment allowlistphase8 включаетsrc/chat.ts: передатьLLMexplicit source.number=arrayindex+1, ранееpromptтребовал[N]безпередачиномеров. Этоисправлениесущеcтвующегоконтрактафазы5, безновыхфайлов/deps/API. ManualRED: фактическийходнеполучилacceptedassistant; памятьиretrievalсохранились.

Phase8 executionamendment: livequote-copy failed twice, falseanswerнепринят. Независимый reviewapprove numeric selected-source decoder с backend-owned exactfragmentquote, mixedformatreject, legacyvalidatorunchanged. IndependentHTTP REDpositive numericcaseдоimplementation; negativeinvalid/mixed+forgedlegacy сохраняются. Memoryprompt уточнён validJSONexamples; extractionpayload current/history теперьтолькоusertext/id/role, безretrieval/traces. Отдельнаяfull-state diagnosticnetworkкомандаотклоненаautoreview иневыполнялась; этотметодпрекращён.


Приёмка 2026-10-03: фаза8 browser create/settings/ask/sources/task/build/actualClaude/reopen пройдена. Cancel проверен тем же UI против медленного локального LLM-fixture: «Действие остановлено», соединение закрыто, незавершённая память требует retry. Фаза9: packaged arm64 window/история/sources подтверждены; ASAR-helper воспроизводимо не запускался, --no-asar исправил физический путь, packaged helper читает реальную FTS5. Новый вопрос в packaged UI ещё требует live разрешения. Фаза10: независимые ledgers по13turns и driver реализованы, но eval:live не выполнялся: auto-review отклонил передачу сценариев/RAG-фрагментов в Cerebras без отдельного согласия. Запрос пользователю pending; отказ не обходился.

Amendment фазы8: independent ops-review обнаружил потерю истории при delayed HTTP body. Allowlist уже содержит src/server.ts; tests/server.test.ts добавлен для regression. RED сохранил только B, GREEN после fresh snapshot под busy lock сохраняет A,B. Комбинированные ссылки [1,999] также RED→GREEN; неизвестный номер не принимается. Overall strict +41 Node +5 Python PASS. Фаза9 package PASS после --no-asar. Fresh code review не нашёл BLOCKING; ops BLOCKING устранён, три WARN описаны в ACCEPTANCE.

Live amendment 2026-10-03: пользователь явно разрешил Cerebras-передачу synthetic scenarios/RAG-фрагментов и Claude-проверку. Первый прогон выявил unsupported prose, который backend всё равно отбрасывает: provenance проверяется прежде, затем fixed fallback возвращается до сканирования discarded references. Independent regression RED→GREEN. Второй прогон docs13/13 +Claude3/3 structural PASS, review остановился на citation mismatch; DeepWiki реально вернул источник, research unavailable. Semantic review docs нашёл неподтверждённую атрибуцию из предметного индекса; ANSWER prompt ограничен двумя тезисами, буквальным основанием и явным отделением вывода. Format repair теперь получает rejectedAnswer как данные, может исправить content/номера, без лишних попыток; independent HTTP regression RED→GREEN. Это изменения src/chat.ts/tests/chat.test.ts в существующем scope и бюджете, не новый validation subsystem. Финальный live PASS ещё не заявлен.


Приёмка 2026-10-03 после явного разрешения пользователя на live данные: 46 Node+5 Python PASS; обе истории по 13 сообщений, restart и семантическая сверка PASS. Новые русские агенты 7f871ee2/aef6060f прошли по 3 actual Claude cases; старые failed версии сохранены. Единственная BUILD format-repair добавлена по наблюдаемому пустому JSON, до неё независимые два HTTP-теста дали RED; после — GREEN, сетевые/I/O ошибки не повторяются. Дополнение к retries0: только одна format repair для ANSWER и одна для BUILD, без model fallback. Electron packaged RAG-вопрос и отмена в окне подтверждены, фаза 9 закрыта. Фаза 10 остаётся wip: research localhost:8800 недоступен и новый URL ожидается; DeepWiki source подтверждён. Итоговый manual-review отчёт exit1 только из-за research=false; реального видео нет, подготовлен VIDEO.md. Все актуальные факты и ограничения: ACCEPTANCE.md.
