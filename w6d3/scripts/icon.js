// Растеризует icon.svg в PNG 1024×1024 через Electron offscreen:
// qlmanage кладёт SVG на белый фон, и углы иконки в Dock становятся белыми.
const { app, BrowserWindow } = require('electron');
const fs = require('node:fs');
const path = require('node:path');

const [svgPath, pngPath] = process.argv.slice(-2);
app.dock?.hide();
app.whenReady().then(async () => {
  const win = new BrowserWindow({ width: 1024, height: 1024, show: false, frame: false, transparent: true, webPreferences: { offscreen: true } });
  const html = `<body style="margin:0;background:transparent">${fs.readFileSync(svgPath, 'utf8')}</body>`;
  await win.loadURL('data:text/html;charset=utf-8,' + encodeURIComponent(html));
  await new Promise(resolve => setTimeout(resolve, 300));
  const image = (await win.webContents.capturePage()).resize({ width: 1024, height: 1024 });
  fs.writeFileSync(pngPath, image.toPNG());
  app.quit();
}).catch(error => { console.error(error); app.exit(1); });
