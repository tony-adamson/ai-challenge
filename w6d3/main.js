// RAG Lab: окно Electron. RAG — release-бинарник w6d3 дочерним процессом (события JSONL в stdout),
// генерация — mlx_lm.server на localhost:8080. Уже запущенный сервер используем и не трогаем, свой гасим при выходе.
const { app, BrowserWindow, dialog, ipcMain } = require('electron');
const { spawn } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

// Из Finder/Dock нет рабочей папки и PATH терминала: каталог дня зашит в config.json при упаковке.
function readRoot() {
  try { return JSON.parse(fs.readFileSync(path.join(__dirname, 'config.json'), 'utf8')).root; } catch { return __dirname; }
}
const ROOT = readRoot();
const BIN = path.join(ROOT, 'target', 'release', 'w6d3');
const SERVER_BIN = path.join(ROOT, '..', 'w6d1', '.venv', 'bin', 'mlx_lm.server');
const DATA = path.join(ROOT, 'data');
const SERVER = 'http://localhost:8080';
const MODELS = ['qwen', 'gemma'];
// Hugging Face не нужен ни серверу, ни бинарнику: веса и эмбеддинги уже в кэше, работает без Wi-Fi.
const ENV = { ...process.env, HF_HUB_OFFLINE: '1' };

let win = null;
let server = null; // только сервер, запущенный этим приложением
let job = null;    // текущий процесс w6d3 (ask или bench), одновременно один
let quitting = false;
let online = { at: 0, value: false };

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

// Любой ответ api.deepseek.com, даже 404, значит, что сеть есть. Проверка не чаще раза в 15 с.
async function isOnline() {
  if (Date.now() - online.at < 15000) return online.value;
  let value = false;
  try { await fetch('https://api.deepseek.com', { method: 'HEAD', signal: AbortSignal.timeout(3000) }); value = true; } catch {}
  online = { at: Date.now(), value };
  return value;
}

async function startServer() {
  if (await serverUp()) { log('mlx_lm.server уже отвечает на 8080 — используем его'); return; }
  if (!fs.existsSync(SERVER_BIN)) {
    log(`нет ${SERVER_BIN}`);
    dialog.showErrorBox('Нет mlx-lm', `Не найден ${SERVER_BIN}.\nУстанови по w6d1/README.md: python3 -m venv .venv && .venv/bin/pip install mlx-lm`);
    return;
  }
  fs.mkdirSync(DATA, { recursive: true });
  const out = fs.openSync(path.join(DATA, 'mlx_server.log'), 'w');
  server = spawn(SERVER_BIN, ['--model', 'mlx-community/Qwen3.5-9B-MLX-4bit', '--port', '8080'], { env: ENV, stdio: ['ignore', out, out] });
  log(`запущен mlx_lm.server pid=${server.pid}, HF_HUB_OFFLINE=${ENV.HF_HUB_OFFLINE}`);
  server.on('error', error => dialog.showErrorBox('Сервер модели не запустился', error.message));
  server.on('exit', (code, signal) => {
    server = null;
    log(`mlx_lm.server завершился: ${signal ?? 'код ' + code}`);
    // Сервер упал сам (нет весов, нехватка памяти) — без диалога окно ждало бы его вечно.
    if (!quitting) dialog.showErrorBox('Сервер модели остановился', `mlx_lm.server завершился (${signal ?? 'код ' + code}). Подробности — в ${path.join(DATA, 'mlx_server.log')}.`);
  });
}

function run(args) {
  if (job) return 'Уже идёт другой запрос — дождись его или останови.';
  if (!fs.existsSync(BIN)) return `Не найден ${BIN}. Собери: cd w6d3 && cargo build --release`;
  job = spawn(BIN, ['--json', ...args], { cwd: ROOT, env: ENV });
  log(`запуск w6d3 ${args[0]} pid=${job.pid}`);
  let buffer = '', stderr = '', finished = false;
  job.stdout.setEncoding('utf8');
  job.stdout.on('data', chunk => {
    buffer += chunk;
    const lines = buffer.split('\n');
    buffer = lines.pop();
    for (const line of lines.filter(l => l.trim())) {
      try {
        const event = JSON.parse(line);
        if (['done', 'summary', 'error'].includes(event.event)) finished = true;
        send(event);
      } catch { stderr = (stderr + line + '\n').slice(-2000); }
    }
  });
  job.stderr.on('data', chunk => { stderr = (stderr + chunk).slice(-2000); });
  job.on('error', error => { finished = true; send({ event: 'error', message: `w6d3 не запустился: ${error.message}` }); });
  job.on('close', (code, signal) => {
    job = null;
    log(`w6d3 завершился: ${signal ?? 'код ' + code}`);
    if (signal) send({ event: 'stopped' });
    else if (!finished) send({ event: 'error', message: `w6d3 завершился с кодом ${code}: ${stderr.trim().slice(-400) || 'без вывода'}` });
    send({ event: 'idle' });
  });
  return null;
}

function latestBench() {
  try {
    const files = fs.readdirSync(DATA).filter(f => /^bench-.*\.json$/.test(f)).sort();
    if (!files.length) return null;
    const file = files[files.length - 1];
    return { file, report: JSON.parse(fs.readFileSync(path.join(DATA, file), 'utf8')) };
  } catch { return null; }
}

ipcMain.handle('ask', (_, question, model) => {
  if (typeof question !== 'string' || !question.trim() || question.length > 2000) return 'Вопрос пуст или длиннее 2000 символов.';
  if (!MODELS.includes(model)) return 'Неизвестная модель.';
  // Вопрос после «--»: текст вида «-1 …» или «--help» не разбирается как флаг.
  return run(['ask', '--model', model, '--', question.trim()]);
});
ipcMain.handle('bench', (_, runs) => run(['bench', '--runs', String(runs === 1 ? 1 : 3)]));
ipcMain.handle('stop', () => { job?.kill('SIGTERM'); });
ipcMain.handle('latest', () => latestBench());
ipcMain.handle('status', async () => ({ server: await serverUp(), online: await isOnline(), binary: fs.existsSync(BIN), busy: !!job }));

app.whenReady().then(async () => {
  log(`старт: root=${ROOT}, бинарник ${fs.existsSync(BIN) ? 'найден' : 'НЕ найден'}: ${BIN}`);
  await startServer();
  win = new BrowserWindow({
    width: 1280, height: 860, minWidth: 960, minHeight: 640, title: 'RAG Lab', backgroundColor: '#F7F4ED',
    webPreferences: { preload: path.join(__dirname, 'preload.js') },
  });
  win.loadFile(path.join(__dirname, 'index.html'));
});
app.on('window-all-closed', () => app.quit());
app.on('will-quit', () => { quitting = true; job?.kill(); server?.kill(); });
