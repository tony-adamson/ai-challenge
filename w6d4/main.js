// Книжный исследователь: окно Electron. Агент — release-бинарник w6d4 дочерним процессом (события JSONL в stdout),
// модель — свой mlx_lm.server на localhost:8080. Профиль меняет флаги сервера (KV-кэш), поэтому смена профиля
// перезапускает только свой сервер. Чужой сервер на 8080 не трогаем — говорим об этом в окне.
const { app, BrowserWindow, dialog, ipcMain } = require('electron');
const { spawn } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

// Из Finder/Dock нет рабочей папки и PATH терминала: каталог дня зашит в config.json при упаковке.
function readRoot() {
  try { return JSON.parse(fs.readFileSync(path.join(__dirname, 'config.json'), 'utf8')).root; } catch { return __dirname; }
}
const ROOT = readRoot();
const BIN = path.join(ROOT, 'target', 'release', 'w6d4');
const SERVER_BIN = path.join(ROOT, '..', 'w6d1', '.venv', 'bin', 'mlx_lm.server');
const DATA = path.join(ROOT, 'data');
const SERVER = 'http://localhost:8080';
// Модель дня — одна; бенч обеих лежит в README.
const MODEL = { key: 'qwen', repo: 'models--mlx-community--Qwen3.5-9B-MLX-4bit' };
// KV-кэш в 8 бит с первого токена — флаг сервера, не запроса.
const SERVER_FLAGS = { baseline: [], optimized: ['--kv-bits', '8', '--quantized-kv-start', '0'] };
// Hugging Face не нужен: веса уже в кэше, работает без Wi-Fi.
const ENV = { ...process.env, HF_HUB_OFFLINE: '1' };

let win = null;
let server = null;       // только сервер, запущенный этим приложением
let profile = null;      // профиль, с которым запущен свой сервер
let starting = false;
let foreign = false;     // на 8080 отвечает чужой процесс
let job = null;          // текущий процесс w6d4 ask, одновременно один
let quitting = false;

function log(line) {
  try {
    fs.mkdirSync(DATA, { recursive: true });
    fs.appendFileSync(path.join(DATA, 'app.log'), `${new Date().toISOString()} ${line}\n`);
  } catch {}
}

function send(event) {
  if (win && !win.isDestroyed()) win.webContents.send('event', event);
}

async function serverUp() {
  try { return (await fetch(SERVER + '/v1/models', { signal: AbortSignal.timeout(2000) })).ok; } catch { return false; }
}

// Путь снапшота напрямую: с HF_HUB_OFFLINE=1 загрузка по имени репозитория падает.
function snapshot() {
  const dir = path.join(process.env.HF_HUB_CACHE || path.join(os.homedir(), '.cache', 'huggingface', 'hub'), MODEL.repo, 'snapshots');
  const first = fs.existsSync(dir) ? fs.readdirSync(dir)[0] : null;
  return first ? path.join(dir, first) : null;
}

// Сначала забываем свой сервер, потом гасим: обработчик exit видит, что остановка намеренная, и не показывает диалог.
function stopServer() {
  return new Promise(resolve => {
    const child = server;
    if (!child) return resolve();
    server = null; profile = null;
    child.once('exit', () => resolve());
    child.kill();
  });
}

// Перезапуск своего сервера под профиль. Возвращает текст ошибки или null.
async function startServer(next) {
  if (server && profile === next) return null;
  starting = true; send({ event: 'server', starting: true, profile: next });
  try {
    await stopServer();
    if (await serverUp()) {
      foreign = true;
      log('на 8080 отвечает чужой сервер — не трогаем');
      return 'На порту 8080 уже работает чужой сервер модели. Его флаги приложение не меняет, а останавливать чужие процессы не будет — закрой его и перезапусти приложение.';
    }
    foreign = false;
    const snap = snapshot();
    if (!fs.existsSync(SERVER_BIN)) return `Не найден ${SERVER_BIN}. Установи mlx-lm по README w6d1.`;
    if (!snap) return `Нет весов ${MODEL.repo} в кэше Hugging Face — скачай их по README w6d1.`;
    fs.mkdirSync(DATA, { recursive: true });
    const out = fs.openSync(path.join(DATA, 'mlx_server.log'), 'w');
    const args = ['--model', snap, '--port', '8080', ...SERVER_FLAGS[next]];
    const child = spawn(SERVER_BIN, args, { env: ENV, stdio: ['ignore', out, out] });
    fs.closeSync(out); // у дочернего процесса своя копия дескриптора
    server = child; profile = next;
    log(`запущен mlx_lm.server pid=${child.pid} профиль=${next} ${SERVER_FLAGS[next].join(' ')}`);
    child.on('error', error => dialog.showErrorBox('Сервер модели не запустился', error.message));
    child.on('exit', (code, signal) => {
      log(`mlx_lm.server pid=${child.pid} завершился: ${signal ?? 'код ' + code}`);
      if (server !== child) return; // остановлен намеренно: перезапуск под другой профиль
      server = null; profile = null;
      // Сервер упал сам (нет весов, нехватка памяти) — без диалога окно ждало бы его вечно.
      if (!quitting) dialog.showErrorBox('Сервер модели остановился', `mlx_lm.server завершился (${signal ?? 'код ' + code}). Подробности — в ${path.join(DATA, 'mlx_server.log')}.`);
    });
    for (let i = 0; i < 180; i++) {
      if (await serverUp()) return null;
      if (server !== child) return 'Сервер модели завершился при старте — см. data/mlx_server.log.';
      await new Promise(resolve => setTimeout(resolve, 1000));
    }
    return 'Сервер модели не ответил за 3 минуты — см. data/mlx_server.log.';
  } finally {
    starting = false; send({ event: 'server', starting: false, profile });
  }
}

function run(args) {
  if (job) return 'Уже идёт другой запрос — дождись его или останови.';
  if (!fs.existsSync(BIN)) return `Не найден ${BIN}. Собери: cd w6d4 && cargo build --release`;
  job = spawn(BIN, ['--json', ...args], { cwd: ROOT, env: ENV });
  log(`запуск w6d4 ${args.join(' ').slice(0, 120)} pid=${job.pid}`);
  let buffer = '', stderr = '', finished = false;
  job.stdout.setEncoding('utf8');
  job.stdout.on('data', chunk => {
    buffer += chunk;
    const lines = buffer.split('\n');
    buffer = lines.pop();
    for (const line of lines.filter(l => l.trim())) {
      try {
        const event = JSON.parse(line);
        if (['done', 'error'].includes(event.event)) finished = true;
        send(event);
      } catch { stderr = (stderr + line + '\n').slice(-2000); }
    }
  });
  job.stderr.on('data', chunk => { stderr = (stderr + chunk).slice(-2000); });
  job.on('error', error => { finished = true; send({ event: 'error', message: `w6d4 не запустился: ${error.message}` }); });
  job.on('close', (code, signal) => {
    job = null;
    log(`w6d4 завершился: ${signal ?? 'код ' + code}`);
    if (signal) send({ event: 'stopped' });
    else if (!finished) send({ event: 'error', message: `w6d4 завершился с кодом ${code}: ${stderr.trim().slice(-400) || 'без вывода'}` });
    send({ event: 'idle' });
  });
  return null;
}

// Последний отчёт bench каждого профиля для модели дня.
function latestBench() {
  const out = {};
  for (const p of Object.keys(SERVER_FLAGS)) {
    try {
      const files = fs.readdirSync(DATA).filter(f => f.startsWith(`bench-${p}-${MODEL.key}-`) && f.endsWith('.json')).sort();
      const file = files.at(-1);
      if (file) out[p] = { file, report: JSON.parse(fs.readFileSync(path.join(DATA, file), 'utf8')) };
    } catch {}
  }
  return out;
}

ipcMain.handle('ask', (_, question) => {
  if (typeof question !== 'string' || !question.trim() || question.length > 2000) return 'Вопрос пуст или длиннее 2000 символов.';
  if (starting) return 'Сервер модели ещё запускается.';
  // Вопрос после «--»: текст вида «-1 …» не разбирается как флаг.
  return run(['ask', '--profile', profile ?? 'optimized', '--', question.trim()]);
});
ipcMain.handle('profile', async (_, next) => {
  if (!SERVER_FLAGS[next]) return 'Неизвестный профиль.';
  if (job) return 'Дождись ответа, потом меняй профиль.';
  return startServer(next);
});
ipcMain.handle('stop', () => { job?.kill('SIGTERM'); });
ipcMain.handle('latest', () => latestBench());
ipcMain.handle('status', async () => ({ server: !starting && await serverUp(), starting, foreign, profile, binary: fs.existsSync(BIN), busy: !!job }));

app.whenReady().then(async () => {
  log(`старт: root=${ROOT}, бинарник ${fs.existsSync(BIN) ? 'найден' : 'НЕ найден'}: ${BIN}`);
  win = new BrowserWindow({
    width: 1280, height: 860, minWidth: 960, minHeight: 640, title: 'Книжный исследователь', backgroundColor: '#F7F4ED',
    webPreferences: { preload: path.join(__dirname, 'preload.js') },
  });
  win.loadFile(path.join(__dirname, 'index.html'));
  const error = await startServer('optimized');
  if (error) send({ event: 'error', message: error });
});
app.on('window-all-closed', () => app.quit());
app.on('will-quit', () => { quitting = true; job?.kill(); server?.kill(); });
