import hashlib
import json
import pathlib
import sqlite3
import subprocess
import sys
import tempfile
import unittest


class RetrievalTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='w5d5-rag-')
        self.root = pathlib.Path(self.temp.name)
        self.db = self.root / 'books.sqlite3'
        with sqlite3.connect(self.db) as connection:
            connection.execute('CREATE VIRTUAL TABLE chunks USING fts5(source UNINDEXED,title UNINDEXED,category UNINDEXED,format UNINDEXED,page_start UNINDEXED,page_end UNINDEXED,chunk_no UNINDEXED,word_count UNINDEXED,text)')
            connection.executemany('INSERT INTO chunks VALUES (?,?,?,?,?,?,?,?,?)', [
                ('guide.pdf', 'Guide', 'agents', 'pdf', 3, 4, 7, 5, 'agent memory evidence planning tools'),
                ('manual.epub', 'Manual', 'agents', 'epub', None, None, 2, 3, 'agent retrieval references'),
            ])
        self.docs = self.root / 'docs'
        self.docs.mkdir()
        self.helper = pathlib.Path(__file__).resolve().parents[1] / 'scripts' / 'search_kb.py'

    def tearDown(self):
        self.temp.cleanup()

    def run_helper(self, query):
        return subprocess.run([sys.executable, str(self.helper), '--db', str(self.db), '--query', query, '--roots', json.dumps([{'path': str(self.docs), 'kind': 'custom'}])], text=True, capture_output=True, timeout=15)

    def sources(self, query):
        result = self.run_helper(query)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)['sources']

    def test_and_to_or_fallback_preserves_literal_text_and_database(self):
        before = hashlib.sha256(self.db.read_bytes()).digest()
        sources = self.sources('agent absentword')
        self.assertEqual({source['id'] for source in sources}, {'guide.pdf#7', 'manual.epub#2'})
        guide = next(source for source in sources if source['id'] == 'guide.pdf#7')
        self.assertEqual(guide['title'], 'Guide')
        self.assertEqual(guide['text'], 'agent memory evidence planning tools')
        self.assertEqual(guide['pages'], '3–4')
        epub = next(source for source in sources if source['id'] == 'manual.epub#2')
        self.assertNotIn('pages', epub)
        self.assertEqual(hashlib.sha256(self.db.read_bytes()).digest(), before)

    def test_hidden_runtime_directories_and_outside_symlink_are_excluded(self):
        (self.docs / 'README.md').write_text('# Public\nagent public evidence', encoding='utf8')
        for name in ['.hidden', 'data', 'state', 'logs', 'ledger', 'reports']:
            directory = self.docs / name
            directory.mkdir()
            (directory / 'secret.md').write_text('agent PRIVATE_MARKER', encoding='utf8')
        (self.docs / '.env').write_text('agent PRIVATE_MARKER', encoding='utf8')
        outside = self.root / 'outside.md'
        outside.write_text('agent PRIVATE_MARKER', encoding='utf8')
        (self.docs / 'escaped.md').symlink_to(outside)
        sources = self.sources('agent')
        docs = [source for source in sources if source['type'] == 'document']
        self.assertEqual(len(docs), 1)
        self.assertIn('public evidence', docs[0]['text'])
        self.assertNotIn('PRIVATE_MARKER', json.dumps(sources))

    def test_separate_quotas_and_total_character_limit(self):
        with sqlite3.connect(self.db) as connection:
            connection.executemany('INSERT INTO chunks VALUES (?,?,?,?,?,?,?,?,?)', [(f'book{i}.pdf', 'Book', '', 'pdf', 1, 1, 1, 100, 'agent ' * 300) for i in range(8)])
        for index in range(8):
            (self.docs / f'doc{index}.md').write_text('# Section\n' + 'agent ' * 300, encoding='utf8')
        sources = self.sources('agent')
        self.assertEqual(sum(source['type'] == 'book' for source in sources), 4)
        self.assertEqual(sum(source['type'] == 'document' for source in sources), 4)
        self.assertLessEqual(sum(len(source['text']) for source in sources), 20000)

    def test_oversized_document_fails_visibly(self):
        (self.docs / 'huge.md').write_text('agent ' + 'x' * (2 * 1024 * 1024), encoding='utf8')
        self.assertNotEqual(self.run_helper('agent').returncode, 0)

    def test_explicit_excluded_directory_or_file_cannot_bypass_root_policy(self):
        hidden = self.docs / '.hidden'
        hidden.mkdir()
        secret = hidden / 'secret.md'
        secret.write_text('agent PRIVATE_MARKER', encoding='utf8')
        logs = self.docs / 'logs'
        logs.mkdir()
        (logs / 'secret.md').write_text('agent PRIVATE_MARKER', encoding='utf8')
        for selected in [hidden, secret, logs]:
            with self.subTest(selected=str(selected)):
                result = subprocess.run([sys.executable, str(self.helper), '--db', str(self.db), '--query', 'agent', '--roots', json.dumps([{'path': str(selected), 'kind': 'custom'}])], text=True, capture_output=True, timeout=15)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn('PRIVATE_MARKER', result.stdout)


if __name__ == '__main__':
    unittest.main()
