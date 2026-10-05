import { createHash } from 'node:crypto';
import { Client, StreamableHTTPClientTransport } from '@modelcontextprotocol/client';
import { validateSettings, type Settings, type Source } from './store.js';

export class LlmFormatError extends Error {}

const providers = {
  cerebras: { url: 'https://api.cerebras.ai/v1/chat/completions', key: 'CEREBRAS_API_KEY' },
  deepseek: { url: 'https://api.deepseek.com/chat/completions', key: 'DEEPSEEK_API_KEY' },
  openrouter: { url: 'https://openrouter.ai/api/v1/chat/completions', key: 'OPENROUTER_API_KEY' },
};
const object = (value: unknown): value is Record<string, unknown> => value !== null && typeof value === 'object' && !Array.isArray(value);
const deadline = (signal: AbortSignal | undefined, ms: number): AbortSignal => AbortSignal.any([...(signal ? [signal] : []), AbortSignal.timeout(ms)]);

// Count bytes before decoding, including chunked responses without Content-Length.
function boundedResponse(response: Response): Response {
  if (Number(response.headers.get('content-length')) > 1024 * 1024) {
    void response.body?.cancel();
    throw new Error('Response exceeds 1 MiB');
  }
  if (!response.body) return response;
  let bytes = 0;
  const body = response.body.pipeThrough(new TransformStream<Uint8Array, Uint8Array>({
    transform(chunk, controller) {
      bytes += chunk.byteLength;
      if (bytes > 1024 * 1024) throw new Error('Response exceeds 1 MiB');
      controller.enqueue(chunk);
    },
  }));
  return new Response(body, { status: response.status, statusText: response.statusText, headers: response.headers });
}

export async function llmJson(settings: Settings, messages: { role: 'system' | 'user' | 'assistant'; content: string }[], signal?: AbortSignal, config?: { url?: string; key?: string }): Promise<unknown> {
  validateSettings(settings);
  const provider = providers[settings.provider];
  const key = config?.key ?? process.env[provider.key];
  if (!key?.trim()) throw new Error(`Missing ${provider.key}`);
  const wire: typeof messages = [];
  for (const message of messages) {
    const previous = wire.at(-1);
    if (message.role === 'system' && previous?.role === 'system') previous.content += `\n\n${message.content}`;
    else wire.push({ role: message.role, content: message.content });
  }
  const body: Record<string, unknown> = { model: settings.model, messages: wire, temperature: settings.temperature, stream: false, response_format: { type: 'json_object' } };
  if (settings.provider === 'cerebras') {
    let effort = ['none', 'low', 'medium', 'high'].includes(settings.reasoning) ? settings.reasoning : 'none';
    if (settings.model.startsWith('gpt-oss') && effort === 'none') effort = 'low';
    Object.assign(body, { max_completion_tokens: 4096, reasoning_effort: effort });
  } else if (settings.provider === 'deepseek') {
    Object.assign(body, { max_tokens: 4096, thinking: { type: settings.reasoning === 'none' ? 'disabled' : 'enabled' } });
    if (settings.reasoning !== 'none') body.reasoning_effort = settings.reasoning;
  } else {
    const maxTokens = settings.model === 'openai/gpt-3.5-turbo-0613' ? 1024 : settings.model === 'gryphe/mythomax-l2-13b' ? 2048 : 4096;
    Object.assign(body, { max_tokens: maxTokens, usage: { include: true }, plugins: [{ id: 'context-compression', enabled: false }] });
  }
  const requestSignal = deadline(signal, 60_000);
  let stage = 'request';
  try {
    requestSignal.throwIfAborted();
    const response = await fetch(config?.url ?? provider.url, { method: 'POST', redirect: 'error', headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${key}` }, body: JSON.stringify(body), signal: requestSignal });
    if (!response.ok) { await response.body?.cancel(); throw new Error(`HTTP ${response.status}`); }
    stage = 'response';
    const value: unknown = await boundedResponse(response).json();
    if (!object(value) || !Array.isArray(value.choices)) throw new Error('Invalid response');
    const choice: unknown = value.choices[0];
    if (!object(choice) || !object(choice.message) || typeof choice.message.content !== 'string' || choice.finish_reason === 'length') throw new Error('Invalid response');
    let result: unknown;
    try { result = JSON.parse(choice.message.content); } catch { throw new LlmFormatError('LLM JSON response invalid'); }
    requestSignal.throwIfAborted();
    return result;
  } catch (error) {
    if (error instanceof LlmFormatError) throw error;
    const status = error instanceof Error && /^HTTP \d{3}$/.test(error.message) ? ` ${error.message}` : '';
    throw new Error(`LLM ${stage} failed${requestSignal.aborted ? ' (cancelled or timed out)' : status}`);
  }
}

export async function mcpSources(server: 'research' | 'deepwiki', tool: string, args: Record<string, unknown>, signal?: AbortSignal, config?: { url?: string }): Promise<{ sources: Source[]; trace: Record<string, unknown> }> {
  if (server === 'research') {
    if (tool !== 'search_repositories' || typeof args.query !== 'string' || !args.query.trim() || args.query.length > 256 || Object.keys(args).some(key => !['query', 'limit'].includes(key)) || (args.limit !== undefined && (!Number.isInteger(args.limit) || Number(args.limit) < 1 || Number(args.limit) > 5))) throw new Error('Forbidden MCP tool or arguments');
  } else if (server !== 'deepwiki' || tool !== 'ask_wiki_question' || typeof args.repoName !== 'string' || !/^[\w.-]+\/[\w.-]+$/.test(args.repoName) || typeof args.question !== 'string' || !args.question.trim() || args.question.length > 4000 || Object.keys(args).some(key => !['repoName', 'question'].includes(key))) throw new Error('Forbidden MCP tool or arguments');
  const url = config?.url ?? (server === 'research' ? process.env.WATCH_MCP_URL || 'http://127.0.0.1:8800/mcp' : process.env.DEEPWIKI_MCP_URL || 'https://mcp.deepwiki.com/mcp');
  let endpoint: URL;
  try { endpoint = new URL(url); if (!['http:', 'https:'].includes(endpoint.protocol)) throw new Error(); }
  catch { throw new Error(`MCP ${server} configuration failed`); }
  const requestSignal = deadline(signal, 20_000);
  const client = new Client({ name: 'w5d5-rag-studio', version: '1.0.0' });
  const transport = new StreamableHTTPClientTransport(endpoint, {
    requestInit: { redirect: 'error' },
    reconnectionOptions: { maxRetries: 0, maxReconnectionDelay: 0, initialReconnectionDelay: 0, reconnectionDelayGrowFactor: 1 },
    fetch: async (input, init) => boundedResponse(await fetch(input, { ...init, redirect: 'error', signal: AbortSignal.any([requestSignal, ...(init?.signal ? [init.signal] : []), AbortSignal.timeout(20_000)]) })),
  });
  const trace: Record<string, unknown> = { server, tool, args: { ...args }, sourceIds: [] };
  let stage = 'connect';
  try {
    requestSignal.throwIfAborted();
    await client.connect(transport, { signal: requestSignal, timeout: 20_000 });
    stage = 'list';
    const catalog = await client.listTools({}, { signal: requestSignal, timeout: 20_000 });
    if (!catalog.tools.some(item => item.name === tool)) throw new Error('Tool unavailable');
    stage = 'call';
    const result = await client.callTool({ name: tool, arguments: args }, { signal: requestSignal, timeout: 20_000 });
    if (result.isError) throw new Error('Tool failed');
    stage = 'result';
    const sources: Source[] = [];
    const add = (title: string, text: string, sourceUrl: string) => {
      if (!text || sources.length >= 4) return;
      const snippet = text.slice(0, 4000);
      const id = `${server}-${createHash('sha256').update(JSON.stringify({ server, args, title, sourceUrl, snippet })).digest('hex').slice(0, 24)}`;
      sources.push({ id, type: server, title, url: sourceUrl, text: snippet });
    };
    if (server === 'research') {
      let value: unknown = result.structuredContent;
      if (!object(value) || !Array.isArray(value.repositories)) {
        const text = result.content.find(item => item.type === 'text');
        value = text?.type === 'text' ? JSON.parse(text.text) : null;
      }
      if (!object(value) || !Array.isArray(value.repositories)) throw new Error('Invalid repository metadata');
      for (const repo of value.repositories) {
        if (!object(repo) || typeof repo.name !== 'string' || typeof repo.url !== 'string' || !/^https:\/\/github\.com\/[\w.-]+\/[\w.-]+\/?$/.test(repo.url)) throw new Error('Invalid repository provenance');
        add(repo.name, JSON.stringify(repo), repo.url);
      }
      if (typeof value.search_id === 'string') trace.searchId = value.search_id;
    } else {
      for (const content of result.content) if (content.type === 'text') add(String(args.repoName), content.text, `https://deepwiki.com/${String(args.repoName)}`);
    }
    requestSignal.throwIfAborted();
    trace.sourceIds = sources.map(source => source.id);
    return { sources, trace };
  } catch {
    const message = `MCP ${server} ${stage} failed${requestSignal.aborted ? ' (cancelled or timed out)' : ''}`;
    throw Object.assign(new Error(message), { trace: { ...trace, error: message } });
  } finally {
    try { await client.close(); } finally { await transport.close(); }
  }
}
