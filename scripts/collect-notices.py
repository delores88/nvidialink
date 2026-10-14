"""Collect license texts from the exact Cargo-resolved dependency sources."""
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / 'dist/notices'
OUT.mkdir(parents=True, exist_ok=True)
SUPPLEMENTS = ROOT / 'vendor/third-party-notices'
provenance = json.loads((SUPPLEMENTS / 'PROVENANCE.json').read_text(encoding='utf-8'))
supplements = {item['name']: item for item in provenance['packages']}
packages = {}
for manifest, target in [('Cargo.toml', 'x86_64-pc-windows-msvc'), ('Cargo.toml', 'aarch64-unknown-linux-gnu'), ('src-tauri/Cargo.toml', 'x86_64-pc-windows-msvc')]:
    result = subprocess.run(['cargo', 'metadata', '--offline', '--locked', '--format-version', '1', '--manifest-path', str(ROOT / manifest), '--filter-platform', target], capture_output=True, check=True)
    data = json.loads(result.stdout)
    for package in data['packages']:
        if package.get('source'):
            packages[package['id']] = package

rows, missing = [], []
for package in sorted(packages.values(), key=lambda p: (p['name'], p['version'])):
    source = Path(package['manifest_path']).parent
    name = package['name'] + '-' + package['version']
    texts = []
    origins = []
    for file in source.iterdir():
        if file.is_file() and re.match(r'(?i)^(licen[sc]e|copying|copyright|notice)([._-]|$)', file.name):
            folder = OUT / name
            folder.mkdir(exist_ok=True)
            shutil.copyfile(file, folder / file.name)
            texts.append(name + '/' + file.name)
            origins.append('crate-archive')
    if package.get('license_file'):
        file = source / package['license_file']
        if file.is_file() and not texts:
            folder = OUT / name
            folder.mkdir(exist_ok=True)
            shutil.copyfile(file, folder / 'LICENSE')
            texts.append(name + '/LICENSE')
            origins.append('crate-archive')
    supplement = supplements.get(name)
    if supplement:
        vcs = json.loads((source / '.cargo_vcs_info.json').read_text(encoding='utf-8'))
        if vcs['git']['sha1'] != supplement['sha'] or package.get('license') != supplement['license']:
            raise SystemExit('Pinned notice provenance no longer matches ' + name)
        for item in supplement['files']:
            relative = Path(item['path'])
            if relative.is_absolute() or '..' in relative.parts or relative.parts[0] != name:
                raise SystemExit('Unsafe notice supplement path')
            file = SUPPLEMENTS / relative
            raw = file.read_bytes()
            if hashlib.sha256(raw).hexdigest() != item['sha256']:
                raise SystemExit('Notice supplement digest mismatch: ' + item['path'])
            folder = OUT / name
            folder.mkdir(exist_ok=True)
            shutil.copyfile(file, OUT / relative)
            texts.append(item['path'])
            origins.append(item['basis'])
    if not any(re.match(r'(?i)^(licen[sc]e|copying)([._-]|$)', Path(text).name) for text in texts):
        missing.append(name)
    rows.append({'name': package['name'], 'version': package['version'], 'license': package.get('license'),
                 'selected_license': supplement.get('selected_license') if supplement else None,
                 'source': package['source'], 'texts': sorted(set(texts)), 'text_provenance': sorted(set(origins)),
                 'text_sha256': {text: hashlib.sha256((OUT / text).read_bytes()).hexdigest() for text in sorted(set(texts))}})
status = {'packages': len(rows), 'missing_license_texts': missing, 'pinned_supplements': len(supplements),
          'complete': not missing, 'scope': 'Union of locked Cargo-resolved packages for Windows CLI/link/desktop and Linux ARM64 link; includes build and development dependencies.'}
(OUT / 'COLLECTION-STATUS.json').write_text(json.dumps(status, indent=2) + '\n', encoding='utf-8')
if missing:
    raise SystemExit('Full license-text evidence missing for: ' + ', '.join(missing))
shutil.copyfile(SUPPLEMENTS / 'PROVENANCE.json', OUT / 'SUPPLEMENT-PROVENANCE.json')
shutil.copyfile(SUPPLEMENTS / 'README.md', OUT / 'SUPPLEMENT-README.md')
(OUT / 'DEPENDENCIES.json').write_text(json.dumps(rows, indent=2) + '\n', encoding='utf-8')
print(json.dumps({**status, 'output': str(OUT)}))
