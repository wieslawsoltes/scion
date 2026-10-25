#!/usr/bin/env python3
"""Copy the 34 real prepared edits; never write the retained source store."""
import hashlib
import json
from pathlib import Path
import sqlite3
import zstandard

source = Path('/tmp/scion-codec-diagnostic-results/corpus')
provenance = json.loads((source / 'manifest.json').read_bytes())
store = Path(provenance['input_store'])
database = store / 'metadata.sqlite3'
wal = store / 'metadata.sqlite3-wal'
assert not wal.exists() or wal.stat().st_size == 0, 'source must be checkpointed'
before = {str(p): [p.stat().st_size, p.stat().st_mtime_ns]
          for p in store.iterdir() if p.is_file()}
assert hashlib.sha256(database.read_bytes()).hexdigest() == provenance['database_sha256']
connection = sqlite3.connect(f'file:{database}?mode=ro&immutable=1', uri=True)

def payload(kind, identifier, max_size):
    result = connection.execute(
        'SELECT encoded FROM inline_objects WHERE kind=? AND id=?',
        (kind, bytes.fromhex(identifier))).fetchone()
    assert result is not None, (kind, identifier)
    encoded = result[0]
    assert encoded[:8] == b'SCNOBJ01'
    assert encoded[8] == {'blob': 1, 'tree': 2, 'changeset': 3}[kind]
    assert encoded[17:49].hex() == identifier
    size = int.from_bytes(encoded[9:17], 'little')
    assert size <= max_size
    decoded = zstandard.ZstdDecompressor().decompress(encoded[49:], max_output_size=max_size)
    assert len(decoded) == size
    return decoded

tip = provenance['input_tip']
edits = []
for _ in range(34):
    assert connection.execute('SELECT value FROM records WHERE namespace=? AND key=?',
                              ('repo/benchmark/changesets', tip)).fetchone()
    changeset_raw = payload('changeset', tip, 8192)
    changeset = json.loads(changeset_raw)
    assert len(changeset['parents']) == len(changeset['changes']) == 1
    changed_path, change = next(iter(changeset['changes'].items()))
    assert change['op'] == 'put'
    directory_name, file_name = changed_path.split('/')
    root_id = json.loads(connection.execute('SELECT value FROM records WHERE namespace=? AND key=?',
                         ('repo/benchmark/derived/tree/1', tip)).fetchone()[0])
    root_raw = payload('tree', root_id, 1 << 20)
    root = json.loads(root_raw)
    directory_id = root['entries'][directory_name]['id']
    directory_raw = payload('tree', directory_id, 1 << 20)
    directory = json.loads(directory_raw)
    blob_id = change['entry']['id']
    assert directory['entries'][file_name]['entry'] == change['entry']
    blob_raw = payload('blob', blob_id, 8192)
    assert len(root['entries']) == 1000 and len(directory['entries']) == 100
    edits.append((tip, changeset['parents'][0], changed_path, [
        ('directory_100', 'tree', directory_id, directory_raw),
        ('root_1000', 'tree', root_id, root_raw),
        ('changeset', 'changeset', tip, changeset_raw),
        ('blob_level3_control', 'blob', blob_id, blob_raw),
    ]))
    tip = changeset['parents'][0]
connection.close()
after = {str(p): [p.stat().st_size, p.stat().st_mtime_ns]
         for p in store.iterdir() if p.is_file()}
assert before == after

destination = Path('/tmp/scion-chunk-diagnostic-results/corpus')
destination.mkdir(parents=True, exist_ok=True)
manifest = {'source': provenance, 'source_file_stats_after': after,
            'extraction': 'checkpointed read-only immutable SQLite; source unchanged; Rust verifies complete object IDs before timing',
            'batches': []}
for index, (identifier, parent, path, cases) in enumerate(reversed(edits)):
    batch = {'changeset': identifier, 'parent': parent, 'changed_path': path, 'cases': []}
    for name, kind, object_id, raw in cases:
        filename = f'edit_{index + 1:02}_{name}.bin'
        with (destination / filename).open('xb') as stream:
            stream.write(raw)
        batch['cases'].append({'name': name, 'kind': kind, 'id': object_id,
                               'file': filename, 'decoded_bytes': len(raw),
                               'sha256': hashlib.sha256(raw).hexdigest()})
    manifest['batches'].append(batch)
with (destination / 'manifest.json').open('x') as stream:
    json.dump(manifest, stream, indent=2)
print(json.dumps({'batches': len(manifest['batches']),
                  'payload_bytes': sum(c['decoded_bytes'] for b in manifest['batches'] for c in b['cases']),
                  'source_unchanged': True}))
