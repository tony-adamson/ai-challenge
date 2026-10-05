import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { startServer } from './server.js';
import { newChatService } from './chat.js';

// The root .env supplies existing keys without executing shell statements.
try { process.loadEnvFile(fileURLToPath(new URL('../../../.env', import.meta.url))); }
catch (error) { if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error; }
let service: ReturnType<typeof newChatService>;
const server = await startServer({ root: process.env.W5D5_DATA_ROOT ?? fileURLToPath(new URL('../../data/', import.meta.url)), action: (...args) => service.run(...args) });
service = newChatService(server.store);
console.log(`Local chat: ${server.origin}`);
const browser = spawn('open', [server.bootstrapUrl], { stdio: 'ignore' });
browser.on('error', () => { console.error('Could not open browser'); });
let closing = false;
async function close(): Promise<void> { if (closing) return; closing = true; await server.close(); }
process.once('SIGINT', () => { void close(); });
process.once('SIGTERM', () => { void close(); });
