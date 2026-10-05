import test from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { once } from 'node:events';
import { llmJson, mcpSources } from '../src/clients.js';
import type { Settings } from '../src/store.js';

async function upstream(reply: (body: Record<string, unknown>) => { status?: number; body: unknown }) {
  const calls: { body: Record<string, unknown>; authorization: string | undefined }[] = [];
  const server = createServer(async (request, response) => {
    if (request.method !== 'POST') { response.writeHead(405).end(); return; }
    let raw = '';
    for await (const chunk of request) raw += String(chunk);
    const body = JSON.parse(raw) as Record<string, unknown>;
    calls.push({ body, authorization: request.headers.authorization });
    const result = reply(body);
    response.writeHead(result.status ?? 200, { 'Content-Type': 'application/json' });
    response.end(JSON.stringify(result.body));
  });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const address = server.address() as { port: number };
  return { calls, url: `http://127.0.0.1:${address.port}`, close: () => new Promise<void>((resolve, reject) => server.close(error => error ? reject(error) : resolve())) };
}

const messages = [{ role: 'user' as const, content: 'Return a JSON object' }];
const response = { choices: [{ message: { content: '{"answer":"verified"}' } }] };

test('provider requests preserve explicit model and independently specified wire semantics', async () => {
  const remote = await upstream(() => ({ body: response }));
  try {
    const base = { temperature: 0.3, reasoning: 'none' };
    assert.deepEqual(await llmJson({ ...base, provider: 'cerebras', model: 'qwen-3.8-27b' }, messages, undefined, { url: remote.url, key: 'fixture-key' }), { answer: 'verified' });
    await llmJson({ ...base, provider: 'deepseek', model: 'deepseek-chat' }, messages, undefined, { url: remote.url, key: 'fixture-key' });
    await llmJson({ ...base, provider: 'openrouter', model: 'openai/gpt-3.5-turbo-0613' }, messages, undefined, { url: remote.url, key: 'fixture-key' });
    assert.equal(remote.calls.length, 3);
    const [cerebras, deepseek, router] = remote.calls.map(call => call.body);
    assert.equal(cerebras!.model, 'qwen-3.8-27b');
    assert.equal(cerebras!.max_completion_tokens, 4096);
    assert.equal(cerebras!.reasoning_effort, 'none');
    assert.equal(cerebras!.max_tokens, undefined);
    assert.equal(deepseek!.model, 'deepseek-chat');
    assert.equal(deepseek!.max_tokens, 4096);
    assert.deepEqual(deepseek!.thinking, { type: 'disabled' });
    assert.equal(router!.model, 'openai/gpt-3.5-turbo-0613');
    assert.equal(router!.max_tokens, 1024);
    assert.deepEqual(router!.plugins, [{ id: 'context-compression', enabled: false }]);
    assert.deepEqual(remote.calls.map(call => call.body.stream), [false, false, false]);
    assert.deepEqual(remote.calls.map(call => call.body.response_format), [{ type: 'json_object' }, { type: 'json_object' }, { type: 'json_object' }]);
    assert.deepEqual(remote.calls.map(call => call.authorization), ['Bearer fixture-key', 'Bearer fixture-key', 'Bearer fixture-key']);
    assert.equal(JSON.stringify(remote.calls.map(call => call.body)).includes('fixture-key'), false);
  } finally { await remote.close(); }
});

test('upstream error is visible with no retry or model fallback', async () => {
  const remote = await upstream(() => ({ status: 503, body: { error: 'unavailable' } }));
  const settings: Settings = { provider: 'deepseek', model: 'deepseek-chat', temperature: 0.3, reasoning: 'none' };
  try {
    await assert.rejects(llmJson(settings, messages, undefined, { url: remote.url, key: 'fixture-key' }));
    assert.equal(remote.calls.length, 1);
    assert.equal(remote.calls[0]!.body.model, 'deepseek-chat');
  } finally { await remote.close(); }
});

test('forbidden MCP tool is rejected before any network request', async () => {
  const remote = await upstream(() => ({ body: {} }));
  try {
    await assert.rejects(mcpSources('research', 'watch_create', { query: 'agent' }, undefined, { url: remote.url }));
    assert.equal(remote.calls.length, 0);
  } finally { await remote.close(); }
});

test('real MCP SDK initializes, lists and calls allowed research tool with repository provenance', async () => {
  const tool = { name: 'search_repositories', inputSchema: { type: 'object', properties: { query: { type: 'string' }, limit: { type: 'integer' } }, required: ['query'] } };
  const remote = await upstream(body => {
    const result = body.method === 'initialize' ? { protocolVersion: '2025-03-26', capabilities: { tools: {} }, serverInfo: { name: 'fixture', version: '1' } }
      : body.method === 'tools/list' ? { tools: [tool] }
      : body.method === 'tools/call' ? { content: [{ type: 'text', text: 'repository metadata' }], structuredContent: { repositories: [{ name: 'example/agent', url: 'https://github.com/example/agent', description: 'Read-only agent tools', stars: 17 }] } }
      : {};
    return body.id === undefined ? { status: 202, body: {} } : { body: { jsonrpc: '2.0', id: body.id, result } };
  });
  try {
    const result = await mcpSources('research', 'search_repositories', { query: 'agent', limit: 2 }, undefined, { url: remote.url });
    assert.equal(result.sources.length, 1);
    assert.equal(result.sources[0]!.type, 'research');
    assert.equal(result.sources[0]!.url, 'https://github.com/example/agent');
    assert.match(result.sources[0]!.text, /Read-only agent tools/);
    const calls = remote.calls.filter(call => call.body.method === 'tools/call');
    assert.equal(calls.length, 1);
    assert.deepEqual(calls[0]!.body.params, { name: 'search_repositories', arguments: { query: 'agent', limit: 2 } });
  } finally { await remote.close(); }
});
