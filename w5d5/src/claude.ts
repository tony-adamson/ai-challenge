import { spawn } from 'node:child_process';
import { createHash, randomUUID } from 'node:crypto';
import { readFile, writeFile, rename, mkdir, mkdtemp, readdir, lstat, rm, realpath } from 'node:fs/promises';
import { join, resolve, relative, sep, isAbsolute } from 'node:path';
import { homedir } from 'node:os';
import { artifactStatus, parseVerification } from './artifacts.js';
import type { Store, Chat, Artifact } from './store.js';

const object = (value: unknown): value is Record<string, unknown> => value !== null && typeof value === 'object' && !Array.isArray(value);
const hash = (bytes: Buffer): string => createHash('sha256').update(bytes).digest('hex');
async function safe(root: string, file: string): Promise<string> {
  const base = await realpath(root); const target = resolve(base, file); const local = relative(base, target);
  if (isAbsolute(file) || !local || local === '..' || local.startsWith('..' + sep)) throw new Error('Путь вне комплекта');
  let current = base;
  for (const part of local.split(sep)) { current = join(current, part); if ((await lstat(current)).isSymbolicLink()) throw new Error('Символические ссылки запрещены'); }
  return target;
}
function execute(executable: string, args: string[], cwd: string, signal: AbortSignal, timeoutMs: number): Promise<string> {
  return new Promise((done, reject) => {
    if (signal.aborted) { reject(new Error('Проверка отменена')); return; }
    const env = { ...process.env };
    for (const key of Object.keys(env)) if (/API_KEY|AUTH_TOKEN|OAUTH_TOKEN|SECRET|^AWS_|^AZURE_|^GOOGLE_|^VERTEX_|^ANTHROPIC_|^CLAUDE_CODE_USE_|^OPENAI_|^OPENROUTER_|^DEEPSEEK_|^CEREBRAS_/i.test(key)) delete env[key];
    const child = spawn(executable, args, { cwd, env, shell: false, detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
    let failure: Error | undefined; const stdout: Buffer[] = []; let outBytes = 0; let errBytes = 0;
    const kill = (message: string) => {
      failure ??= new Error(message);
      if (child.pid) { try { process.kill(-child.pid, 'SIGKILL'); } catch { child.kill('SIGKILL'); } }
    };
    const abort = () => kill('Проверка отменена');
    signal.addEventListener('abort', abort, { once: true });
    const timer = setTimeout(() => kill('Истёк срок проверки'), timeoutMs);
    child.stdout.on('data', (bytes: Buffer) => { outBytes += bytes.length; if (outBytes > 1048576) kill('Превышен лимит stdout'); else stdout.push(bytes); });
    child.stderr.on('data', (bytes: Buffer) => { errBytes += bytes.length; if (errBytes > 1048576) kill('Превышен лимит stderr'); });
    child.on('error', () => { failure ??= new Error('Claude CLI недоступен'); });
    child.on('close', code => { clearTimeout(timer); signal.removeEventListener('abort', abort); if (failure || code !== 0) reject(failure ?? new Error('Claude CLI завершился с ошибкой')); else done(Buffer.concat(stdout).toString('utf8')); });
    if (signal.aborted) abort();
  });
}
async function managedPolicy(): Promise<void> {
  const home = homedir();
  const paths = ['/Library/Application Support/ClaudeCode/managed-settings.json', '/Library/Application Support/ClaudeCode/managed-settings.d', '/Library/Managed Preferences/com.anthropic.claudecode.plist', join(home, 'Library/Managed Preferences/com.anthropic.claudecode.plist'), join(home, 'Library/Preferences/com.anthropic.claudecode.plist'), join(home, '.claude/managed-settings.json'), join(home, '.claude/managed-settings.d')];
  for (const location of paths) {
    try { await lstat(location); } catch (error) { if ((error as NodeJS.ErrnoException).code === 'ENOENT') continue; throw error; }
    throw new Error('Managed policy присутствует: отключение hooks не подтверждено');
  }
  try {
    const config: unknown = JSON.parse(await readFile(join(home, '.claude.json'), 'utf8'));
    if (!object(config) || Object.keys(config).some(key => /managed|remote.*policy|policy.*cache/i.test(key))) throw new Error('Remote managed policy не проверена');
  } catch (error) { if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error; }
}
interface CaseReport { id: string; status: 'running' | 'passed' | 'failed' | 'unverified'; answer?: string; model?: string; tools?: unknown[]; reason?: string; expected: string[]; forbidden: string[]; readFiles: string[] }
export async function validateArtifact(store: Store, chat: Chat, artifact: Artifact, signal: AbortSignal, options: { executable?: string; timeoutMs?: number } = {}) {
  const current = await artifactStatus(store, chat, artifact);
  if (current.status === 'stale') return current;
  const bundle = await safe(store.root, artifact.path); const reportFile = join(bundle, 'validation.json');
  try {
    const previous: unknown = JSON.parse(await readFile(reportFile, 'utf8'));
    if (!object(previous) || previous.status !== 'unavailable' || !Array.isArray(previous.cases) || previous.cases.length) return current;
  } catch (error) { if ((error as NodeJS.ErrnoException).code !== 'ENOENT') return current; }
  const fileHashes: Record<string, string> = {};
  for (const file of artifact.files) fileHashes[file.id] = hash(await readFile(await safe(bundle, file.path)));
  const report: { status: string; specHash: string; fileHashes: Record<string, string>; cases: CaseReport[]; reason?: string; limits: string } = { status: 'unverified', specHash: artifact.specHash, fileHashes, cases: [], limits: 'Три синтетических сценария; семантические критерии требуют ручной сверки.' };
  const save = async () => {
    const temporary = join(bundle, 'validation.' + randomUUID() + '.tmp');
    try { await writeFile(temporary, JSON.stringify(report, null, 2) + '\n', { flag: 'wx', mode: 0o600 }); await rename(temporary, reportFile); }
    finally { await rm(temporary, { force: true }); }
  };
  let cwd: string | undefined;
  try {
    await managedPolicy();
    const executable = options.executable ?? process.env.W5D5_CLAUDE_PATH ?? join(homedir(), '.local', 'bin', 'claude');
    const help = await execute(executable, ['--help'], bundle, signal, 10000);
    for (const flag of ['--restricted', '--strict-mcp-config', '--permission-mode', '--agents', '--agent', '--tools', '--settings', '--no-session-persistence', '--output-format', '--verbose']) if (!help.includes(flag)) throw new Error('CLI не поддерживает ' + flag);
    const auth: unknown = JSON.parse(await execute(executable, ['auth', 'status', '--json'], bundle, signal, 10000));
    if (!object(auth) || auth.loggedIn !== true) throw new Error('Авторизация Claude CLI недоступна');
    const agentFile = artifact.files.find(file => file.id === 'agent'); const criteriaFile = artifact.files.find(file => file.id === 'verification');
    if (!agentFile || !criteriaFile) throw new Error('Неполный комплект');
    const saved = await readFile(await safe(bundle, agentFile.path), 'utf8');
    const match = /^---\nname: ([a-z][a-z0-9-]{0,63})\ndescription: (.+)\ntools: Read, Grep, Glob\nmodel: inherit\n---\n\n([\s\S]+)$/.exec(saved);
    if (!match) throw new Error('Неверный формат сохранённого агента');
    const name = match[1]!; const description: unknown = JSON.parse(match[2]!); const prompt = match[3]!.trim();
    if (typeof description !== 'string' || !prompt) throw new Error('Неполный агент');
    const verification = parseVerification(JSON.parse(await readFile(await safe(bundle, criteriaFile.path), 'utf8')));
    const tempRoot = join(store.root, 'test-runs'); await mkdir(tempRoot, { recursive: true }); await safe(store.root, 'test-runs');
    cwd = await mkdtemp(join(tempRoot, artifact.id + '-'));
    for (const file of verification.files) await writeFile(join(cwd, file.name), await readFile(await safe(bundle, join('fixtures', file.name))), { flag: 'wx', mode: 0o600 });
    const original = Object.fromEntries(await Promise.all(verification.files.map(async file => [file.name, hash(await readFile(join(cwd!, file.name)))])));
    const agents = JSON.stringify({ [name]: { description, prompt, tools: ['Read', 'Grep', 'Glob'], model: 'inherit' } });
    for (const criterion of verification.cases) {
      const result: CaseReport = { id: criterion.id, status: 'running', expected: criterion.expected, forbidden: criterion.forbidden, readFiles: criterion.readFiles };
      report.cases.push(result); await save();
      try {
        const output = await execute(executable, ['--restricted', '--tools', 'Read,Grep,Glob', '--allowedTools', 'Read,Grep,Glob', '--permission-mode', 'dontAsk', '--strict-mcp-config', '--mcp-config', '{"mcpServers":{}}', '--settings', '{"disableAllHooks":true,"enableAllProjectMcpServers":false}', '--no-session-persistence', '--disable-slash-commands', '--no-chrome', '--agents', agents, '--agent', name, '--print', '--output-format', 'stream-json', '--verbose', criterion.prompt + '\nRead these files: ' + criterion.readFiles.join(', ')], cwd, signal, Math.min(options.timeoutMs ?? 180000, 180000));
        const events: unknown[] = output.trim().split('\n').filter(line => line.trim()).map(line => JSON.parse(line));
        const read = new Set<string>(); const requested = new Map<string, string>(); const tools: unknown[] = []; result.tools = tools; let completion: Record<string, unknown> | undefined;
        for (const event of events) {
          if (!object(event)) throw new Error('Неверный stream-json');
          if (typeof event.model === 'string') result.model = event.model;
          if (object(event.modelUsage)) result.model = Object.keys(event.modelUsage).join(',');
          if (event.type === 'result') { if (completion) throw new Error('Повтор результата'); completion = event; }
          if (event.type === 'user' && object(event.message) && Array.isArray(event.message.content)) {
            for (const receipt of event.message.content as unknown[]) {
              if (!object(receipt) || receipt.type !== 'tool_result' || typeof receipt.tool_use_id !== 'string') continue;
              const file = requested.get(receipt.tool_use_id);
              if (file && receipt.is_error !== true && (typeof receipt.content === 'string' ? !!receipt.content.trim() : Array.isArray(receipt.content) && receipt.content.length > 0)) read.add(file);
            }
          }
          if (event.type !== 'assistant' || !object(event.message) || !Array.isArray(event.message.content)) continue;
          if (typeof event.message.model === 'string') result.model = event.message.model;
          for (const block of event.message.content as unknown[]) {
            if (!object(block) || block.type !== 'tool_use') continue;
            tools.push(block);
            if (!['Read', 'Grep', 'Glob'].includes(String(block.name)) || !object(block.input)) throw new Error('Запрещённый инструмент');
            const location = block.name === 'Read' ? block.input.file_path : block.input.path === undefined ? '.' : block.input.path;
            if (typeof location !== 'string') throw new Error('Нет подтверждённого пути инструмента');
            const absolute = resolve(cwd, location); const local = relative(cwd, absolute);
            if (local === '..' || local.startsWith('..' + sep) || isAbsolute(local)) throw new Error('Инструмент вышел за рабочий каталог');
            if (block.name === 'Read' && typeof block.id === 'string' && criterion.readFiles.includes(local)) requested.set(block.id, local);
          }
        }
        if (!completion || completion.subtype !== 'success' || completion.is_error !== false || typeof completion.result !== 'string' || !completion.result.trim()) throw new Error('Нет успешного результата Claude');
        result.answer = completion.result;
        if (criterion.readFiles.some(file => !read.has(file)) || criterion.expected.some(value => !result.answer!.includes(value)) || criterion.forbidden.some(value => result.answer!.includes(value))) throw new Error('Предметные критерии не пройдены');
        if ((await readdir(cwd)).sort().join('\0') !== verification.files.map(file => file.name).sort().join('\0')) throw new Error('Набор тестовых файлов изменён');
        for (const file of verification.files) if (hash(await readFile(await safe(cwd, file.name))) !== original[file.name]) throw new Error('Тестовый файл изменён');
        result.status = criterion.manual ? 'unverified' : 'passed';
      } catch (error) { result.status = 'failed'; result.reason = error instanceof SyntaxError ? 'Неверный stream-json' : (error as Error).message; }
      await save();
      if (result.status === 'failed' || signal.aborted) break;
    }
    report.status = report.cases.some(item => item.status === 'failed') ? 'failed' : report.cases.length === 3 && report.cases.every(item => item.status === 'passed') ? 'passed' : 'unverified';
  } catch (error) { report.status = report.cases.length ? 'failed' : 'unavailable'; report.reason = error instanceof SyntaxError ? 'Неверный ответ CLI' : (error as Error).message; }
  finally { if (cwd) await rm(cwd, { recursive: true, force: true }); }
  await save();
  return artifactStatus(store, chat, artifact);
}
