import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { retrieve } from '../src/rag.js';

const python = '/opt/homebrew/bin/python3';
const run = promisify(execFile);

test('Node retrieval executes real Python and returns exact FTS source without changing database', async () => {
  const root = await mkdtemp(join(tmpdir(), 'w5d5-rag-node-'));
  const db = join(root, 'books.sqlite3');
  try {
    await run(python, ['-c', "import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); c.execute('CREATE VIRTUAL TABLE chunks USING fts5(source UNINDEXED,title UNINDEXED,category UNINDEXED,format UNINDEXED,page_start UNINDEXED,page_end UNINDEXED,chunk_no UNINDEXED,word_count UNINDEXED,text)'); c.execute('INSERT INTO chunks VALUES (?,?,?,?,?,?,?,?,?)',('evidence.epub','Evidence','','epub',None,None,9,3,'agent literal evidence')); c.commit(); c.close()", db]);
    const before = await readFile(db);
    const sources = await retrieve('agent missingword', { python, db, roots: [] });
    assert.equal(sources.length, 1);
    assert.equal(sources[0]!.id, 'evidence.epub#9');
    assert.equal(sources[0]!.title, 'Evidence');
    assert.equal(sources[0]!.text, 'agent literal evidence');
    assert.equal(sources[0]!.pages, undefined);
    assert.deepEqual(await readFile(db), before);
    const abort = new AbortController();
    abort.abort();
    await assert.rejects(retrieve('agent', { python, db, roots: [] }, abort.signal));
  } finally { await rm(root, { recursive: true, force: true }); }
});
