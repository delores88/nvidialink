"""Prepare the trusted, hash-bound remote bundle consumed by the installed app."""
import base64
import hashlib
import json
import argparse
from pathlib import Path

ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
parser.add_argument('--controller-only', action='store_true', help='Explicit development bundle without pairing support')
args=parser.parse_args()
files={name:ROOT/'remote'/name for name in ['controller.py','worker.py','prepare.py','proxy.py','install-runtime.py','bootstrap.py','sparkdev-controller.service']}
files.update({'runtime/Dockerfile':ROOT/'runtimes/Dockerfile','runtime/requirements.txt':ROOT/'runtimes/requirements.txt'})
binary=ROOT/'dist/linux-arm64/sparkdev-link'
if not binary.is_file() and not args.controller_only:
    raise SystemExit('A release bundle requires dist/linux-arm64/sparkdev-link')
if binary.is_file():
    files.update({'bin/sparkdev-link':binary,'sparkdev-link.service':ROOT/'crates/sparkdev-link/deploy/sparkdev-link.service'})
files.update({'install-cuda.py':ROOT/'remote/install-cuda.py','runtime/Dockerfile.cuda':ROOT/'runtimes/Dockerfile.cuda'})
bundle={'v':1,'build_runtime':True,'files':{}}
for name,path in files.items():
    raw=path.read_bytes()
    bundle['files'][name]={'sha256':hashlib.sha256(raw).hexdigest(),'data':base64.b64encode(raw).decode('ascii')}
out=ROOT/'dist/sparkdev-bundle.json';out.parent.mkdir(exist_ok=True);out.write_text(json.dumps(bundle,separators=(',',':')),encoding='utf-8')
print(json.dumps({'files':len(files),'link_binary_included':binary.is_file(),'bytes':out.stat().st_size,'sha256':hashlib.sha256(out.read_bytes()).hexdigest()}))
