// Writes ThirdPartyNotices.txt from the license files of the bundled
// third-party code (noVNC and the pako copy it vendors), so the notices
// always match the installed versions.
import * as fs from 'fs';

const novnc = 'node_modules/@novnc/novnc';
const version = JSON.parse(fs.readFileSync(`${novnc}/package.json`, 'utf8')).version;
const read = (f) => fs.readFileSync(`${novnc}/${f}`, 'utf8').trim();
const rule = '='.repeat(78);

const text = `THIRD-PARTY NOTICES

This extension bundles the third-party software listed below (inside
dist/webview.js and dist/extension.js). The files are used unmodified.

TightVNC is NOT included in this package. On Windows the extension downloads
the official TightVNC 2.8.89 installer from https://www.tightvnc.com when
it is first needed (verified by SHA-256). TightVNC is licensed under the GNU
GPL v2; see https://www.tightvnc.com/licensing.php.

${rule}
noVNC ${version}
https://github.com/novnc/noVNC  (source code for the bundled MPL-2.0 files)
${rule}

${read('LICENSE.txt')}

${rule}
Mozilla Public License 2.0 (noVNC core)
${rule}

${read('docs/LICENSE.MPL-2.0')}

${rule}
pako (vendored by noVNC, vendor/pako)
${rule}

${read('vendor/pako/LICENSE')}
`;

fs.writeFileSync('ThirdPartyNotices.txt', text);
console.log(`ThirdPartyNotices.txt written (noVNC ${version})`);
