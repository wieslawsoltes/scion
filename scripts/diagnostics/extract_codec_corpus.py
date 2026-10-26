#!/usr/bin/env python3
"""Copy four encoded payloads from a quiescent fixture without SQLite writes."""
import hashlib
import json
from pathlib import Path
import sqlite3
import zstandard

report_path = Path('/workspace/scratch/481213027244/scion/target/performance-baseline/fixed-default.json')
report = json.loads(report_path.read_bytes())
store = Path(report['work_directory']) / 'scion'
database = store / 'metadata.sqlite3'
wal = store / 'metadata.sqlite3-wal'
assert not wal.exists() or wal.stat().st_size == 0, 'immutable extraction requires a checkpointed fixture'
before = {str(path): (path.stat().st_size, path.stat().st_mtime_ns) for path in store.iterdir() if path.is_file()}
connection = sqlite3.connect(f'file:{database}?mode=ro&immutable=1', uri=True)
tip = report['fixture']['scion']['tip']
assert connection.execute('SELECT value FROM records WHERE namespace=? AND key=?', ('repo/benchmark/changesets', tip)).fetchone()
root = json.loads(connection.execute('SELECT value FROM records WHERE namespace=? AND key=?', ('repo/benchmark/derived/tree/1', tip)).fetchone()[0])

def payload(kind, identifier):
    encoded = connection.execute('SELECT encoded FROM inline_objects WHERE kind=? AND id=?', (kind, bytes.fromhex(identifier))).fetchone()[0]
    assert encoded[:8] == b'SCNOBJ01' and encoded[17:49].hex() == identifier
    raw = zstandard.ZstdDecompressor().decompress(encoded[49:])
    assert len(raw) == int.from_bytes(encoded[9:17], 'little')
    return raw

root_bytes = payload('tree', root)
directory = json.loads(root_bytes)['entries']['dir0000']['id']
directory_bytes = payload('tree', directory)
changeset_bytes = payload('changeset', tip)
changeset = json.loads(changeset_bytes)
blob = changeset['changes'][report['fixture']['path']]['entry']['id']
blob_bytes = payload('blob', blob)
connection.close()
assert {str(path): (path.stat().st_size, path.stat().st_mtime_ns) for path in store.iterdir() if path.is_file()} == before
destination = Path('/tmp/scion-codec-diagnostic-results/corpus')
destination.mkdir(exist_ok=True)
cases = []
for name, kind, identifier, raw in [
    ('directory_100', 'tree', directory, directory_bytes),
    ('root_1000', 'tree', root, root_bytes),
    ('changeset', 'changeset', tip, changeset_bytes),
    ('blob_level3_control', 'blob', blob, blob_bytes),
]:
    path = destination / f'{name}.bin'
    with path.open('xb') as stream:
        stream.write(raw)
    cases.append({'name': name, 'kind': kind, 'id': identifier, 'file': path.name,
                  'decoded_bytes': len(raw), 'raw_sha256': hashlib.sha256(raw).hexdigest()})
manifest = {'source_report': str(report_path), 'source_report_sha256': hashlib.sha256(report_path.read_bytes()).hexdigest(),
            'input_store': str(store), 'input_tip': tip, 'database_sha256': hashlib.sha256(database.read_bytes()).hexdigest(),
            'source_file_stats': before, 'extraction': 'read-only immutable SQLite, WAL size zero, file size/mtime unchanged',
            'cases': cases}
with (destination / 'manifest.json').open('x') as stream:
    json.dump(manifest, stream, indent=2)
print(json.dumps({'copied_payload_bytes': sum(case['decoded_bytes'] for case in cases), 'cases': cases}, indent=2))
