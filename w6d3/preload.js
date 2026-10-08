// Мост окна к main.js: окно не видит ни файловую систему, ни процессы.
const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('rag', {
  ask: (question, model) => ipcRenderer.invoke('ask', question, model),
  bench: runs => ipcRenderer.invoke('bench', runs),
  stop: () => ipcRenderer.invoke('stop'),
  latest: () => ipcRenderer.invoke('latest'),
  status: () => ipcRenderer.invoke('status'),
  onEvent: callback => ipcRenderer.on('event', (_, event) => callback(event)),
});
