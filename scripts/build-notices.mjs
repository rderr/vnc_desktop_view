// Writes ThirdPartyNotices.txt from the license files of the bundled
// third-party code (noVNC and the pako copy it vendors), so the notices
// always match the installed versions.
import * as fs from 'fs';

const novnc = 'node_modules/@novnc/novnc';
const version = JSON.parse(fs.readFileSync(`${novnc}/package.json`, 'utf8')).version;
const read = (f) => fs.readFileSync(`${novnc}/${f}`, 'utf8').trim();
const rule = '='.repeat(78);
const opusDecoderVersion = JSON.parse(fs.readFileSync('node_modules/opus-decoder/package.json', 'utf8')).version;

const text = `THIRD-PARTY NOTICES

This extension bundles the third-party software listed below (inside
dist/webview.js, dist/video.js and dist/extension.js). The files are used unmodified.

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

${rule}
opus-decoder ${opusDecoderVersion} and @wasm-audio-decoders/common (dist/video.js)
https://github.com/eshaz/wasm-audio-decoders
${rule}

MIT License

Copyright (c) Ethan Halsall

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

${rule}
libopus (compiled to WebAssembly inside opus-decoder)
https://opus-codec.org
${rule}

Copyright 2001-2023 Xiph.Org, Skype Limited, Octasic,
                    Jean-Marc Valin, Timothy B. Terriberry,
                    CSIRO, Gregory Maxwell, Mark Borgerding,
                    Erik de Castro Lopo, Mozilla, Amazon

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:

- Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.

- Redistributions in binary form must reproduce the above copyright
notice, this list of conditions and the following disclaimer in the
documentation and/or other materials provided with the distribution.

- Neither the name of Internet Society, IETF or IETF Trust, nor the
names of specific contributors, may be used to endorse or promote
products derived from this software without specific prior written
permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
\`\`AS IS'' AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER
OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
`;

fs.writeFileSync('ThirdPartyNotices.txt', text);
console.log(`ThirdPartyNotices.txt written (noVNC ${version})`);
