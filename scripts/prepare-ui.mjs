import {copyFile,mkdir} from 'node:fs/promises';
await mkdir('ui/vendor',{recursive:true});
await copyFile('node_modules/qrcode-generator/dist/qrcode.mjs','ui/vendor/qrcode.mjs');
await copyFile('node_modules/qrcode-generator/README.md','ui/vendor/QRCODE-README.md');
