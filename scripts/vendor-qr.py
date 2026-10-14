"""Read-only copy of the owner's pinned PhaseForge QR artwork primitives."""
import hashlib
import json
import subprocess
from pathlib import Path

root = Path(__file__).resolve().parents[1]
source = Path('D:/PhaseForge-claude/wt/025-p2')
revision = '944bfcceef5f7969b384d0334b9c612e1244bf2f'
destination = root / 'ui/vendor'
destination.mkdir(parents=True, exist_ok=True)
entries = []
for name in ['sparkLinkQr.mjs', 'sparkParticles.mjs']:
    path = f'frontend/src/lib/{name}'
    original = subprocess.check_output(['git', '-C', str(source), 'show', f'{revision}:{path}'])
    content = original.replace(b"from 'qrcode-generator'", b"from './qrcode.mjs'")
    (destination / name).write_bytes(content)
    entries.append({'source': path, 'destination': f'ui/vendor/{name}', 'original_sha256': hashlib.sha256(original).hexdigest(), 'sha256': hashlib.sha256(content).hexdigest()})
(destination / 'PHASEFORGE-AUTHOR-NOTICE.txt').write_text('Copyright (c) 2026 Ben Van Dorn / DeloresAI.\nOriginally authored for PhaseForge. Ben explicitly authorized reuse and MIT licensing in DGXBridge on 2026-10-13. These copies are covered by the root MIT LICENSE; the PhaseForge repository is unchanged. Third-party components keep their own notices.\n', encoding='utf-8')
(root / 'docs/qr-provenance.json').write_text(json.dumps({'repository': 'https://github.com/delores88/PhaseForge', 'revision': revision, 'authorization': 'Owner requested reuse of PhaseForge QR setup and explicitly required MIT licensing for this project on 2026-10-13.', 'license': 'MIT', 'modifications': 'Only the qrcode-generator import changed to bundled local ESM. Vanilla DOM adapter is original DGXBridge code.', 'files': entries}, indent=2) + '\n', encoding='utf-8')
