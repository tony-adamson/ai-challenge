// Мост окна к main.js: окно не видит ни файловую систему, ни процессы.
const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('lab', {
  ask: question => ipcRenderer.invoke('ask', question),
  profile: name => ipcRenderer.invoke('profile', name),
  stop: () => ipcRenderer.invoke('stop'),
  latest: () => ipcRenderer.invoke('latest'),
  status: () => ipcRenderer.invoke('status'),
  onEvent: callback => ipcRenderer.on('event', (_, event) => callback(event)),
});
