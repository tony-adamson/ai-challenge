// Окно приложения: чат лежит в chat.html и сам обращается к mlx_lm.server на localhost:8080.
// Если сервер не запущен, приложение поднимает его само и останавливает при выходе.
const { app, BrowserWindow, dialog } = require('electron');
const { spawn } = require('node:child_process');
const { existsSync } = require('node:fs');
const path = require('node:path');

const MODEL = 'mlx-community/Qwen3.5-9B-MLX-4bit';
const SERVER_BIN = path.join(__dirname, '.venv', 'bin', 'mlx_lm.server');
let server = null; // только сервер, запущенный этим приложением
let quitting = false;

async function serverRunning() {
  try { return (await fetch('http://localhost:8080/v1/models')).ok; } catch { return false; }
}

async function startServer() {
  if (await serverRunning()) return;
  if (!existsSync(SERVER_BIN)) {
    dialog.showErrorBox('Нет mlx-lm', 'Не найден .venv/bin/mlx_lm.server. Установи его по README: python3 -m venv .venv && .venv/bin/pip install mlx-lm');
    return;
  }
  server = spawn(SERVER_BIN, ['--model', MODEL, '--port', '8080'], { stdio: 'inherit' });
  server.on('error', error => dialog.showErrorBox('Сервер модели не запустился', error.message));
  server.on('exit', (code, signal) => {
    server = null;
    // Выход до закрытия приложения — сервер упал сам (нет весов без сети, нехватка памяти); без диалога окно ждало бы его вечно.
    if (!quitting) dialog.showErrorBox('Сервер модели остановился', `mlx_lm.server завершился (${signal ?? 'код ' + code}). Подробности — в терминале, где запущен npm start.`);
  });
}

app.whenReady().then(async () => {
  await startServer();
  const window = new BrowserWindow({ width: 1180, height: 820, minWidth: 720, minHeight: 560, title: 'Local LLM', backgroundColor: '#fbfcf8' });
  window.loadFile(path.join(__dirname, 'chat.html'));
});
app.on('window-all-closed', () => app.quit());
app.on('will-quit', () => { quitting = true; server?.kill(); });
