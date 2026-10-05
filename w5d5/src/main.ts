import { app, BrowserWindow, dialog, shell } from 'electron';
import { startServer } from './server.js';
import { newChatService } from './chat.js';

const courseRoot = '/Users/admin/MyProjects/ai_challenge';
let server: Awaited<ReturnType<typeof startServer>> | undefined;
let window: BrowserWindow | undefined;
let quitting = false;
let shutdown: Promise<void> | undefined;

function openExternal(target: string): void {
  try {
    const url = new URL(target);
    if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password) return;
    void shell.openExternal(url.href).catch(() => dialog.showErrorBox('Ошибка ссылки', 'Не удалось открыть внешний браузер.'));
  } catch { /* Invalid links cannot leave the application. */ }
}

async function stop(): Promise<void> {
  if (shutdown) return shutdown;
  quitting = true;
  shutdown = (async () => {
    try { await server?.close(); }
    catch { dialog.showErrorBox('Ошибка завершения', 'Не удалось корректно закрыть локальный сервер.'); }
    finally { app.quit(); }
  })();
  return shutdown;
}

if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  app.on('second-instance', () => {
    if (window?.isMinimized()) window.restore();
    window?.focus();
  });
  app.on('before-quit', event => {
    if (!quitting) { event.preventDefault(); void stop(); }
  });
  app.on('window-all-closed', () => { void stop(); });
  process.once('SIGINT', () => { void stop(); });
  process.once('SIGTERM', () => { void stop(); });

  void app.whenReady().then(async () => {
    try {
      try { process.loadEnvFile(process.env.W5D5_ENV_FILE ?? `${courseRoot}/.env`); }
      catch (error) { if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error; }
      process.env.W5D5_COURSE_ROOT ??= courseRoot;
      let service: ReturnType<typeof newChatService>;
      server = await startServer({ root: process.env.W5D5_DATA_ROOT ?? `${courseRoot}/w5d5/data`, action: (...args) => service.run(...args) });
      service = newChatService(server.store);
      if (quitting) { await server.close(); return; }
      window = new BrowserWindow({ width: 1240, height: 860, title: 'RAG Studio', webPreferences: { sandbox: true, contextIsolation: true, nodeIntegration: false } });
      const origin = server.origin;
      const sameOrigin = (target: string): boolean => { try { return new URL(target).origin === origin; } catch { return false; } };
      window.webContents.on('will-navigate', (event, target) => {
        if (!sameOrigin(target)) { event.preventDefault(); openExternal(target); }
      });
      window.webContents.on('will-redirect', (event, target) => { if (!sameOrigin(target)) event.preventDefault(); });
      window.webContents.setWindowOpenHandler(({ url }) => { if (!sameOrigin(url)) openExternal(url); return { action: 'deny' }; });
      window.webContents.session.setPermissionRequestHandler((_contents, _permission, callback) => callback(false));
      window.webContents.session.setPermissionCheckHandler(() => false);
      window.on('closed', () => { window = undefined; });
      await window.loadURL(server.bootstrapUrl);
    } catch {
      dialog.showErrorBox('Ошибка запуска', 'Не удалось открыть чат. Проверь .env, доступность каталога данных и отсутствие другого процесса на том же W5D5_DATA_ROOT.');
      await stop();
    }
  });
}
