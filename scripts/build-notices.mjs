// Writes ThirdPartyNotices.txt for the bundled third-party code (opus-decoder
// in the viewer, and the Rust crates in the desktop server), so the notices
// always match the installed versions.
import * as fs from 'fs';
import { serverDependencies } from './server-notices.mjs';

const rule = '='.repeat(78);
const opusDecoderVersion = JSON.parse(fs.readFileSync('node_modules/opus-decoder/package.json', 'utf8')).version;

const text = `THIRD-PARTY NOTICES

This extension bundles the third-party software listed below (inside
dist/video.js, and in Windows, Linux and macOS packages bin/desktop-server). The files
are used unmodified.

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

// NVENC API definitions (from nvEncodeAPI.h) compiled into the Linux server.
const nvencText = `
${rule}
NVIDIA Video Codec SDK API header nvEncodeAPI.h (Linux packages, bin/desktop-server)
https://github.com/FFmpeg/nv-codec-headers
${rule}

Copyright (c) 2010-2023 NVIDIA Corporation

Permission is hereby granted, free of charge, to any person
obtaining a copy of this software and associated documentation
files (the "Software"), to deal in the Software without
restriction, including without limitation the rights to use,
copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the software, and to permit persons to whom the
software is furnished to do so, subject to the following
conditions:

The above copyright notice and this permission notice shall be
included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES
OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT
HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR
OTHER DEALINGS IN THE SOFTWARE.
`;

// VA-API definitions (from libva's headers) compiled into the Linux server.
const vaText = `
${rule}
libva 2.14 headers va/va.h, va/va_enc_h264.h (Linux packages, bin/desktop-server)
https://github.com/intel/libva
${rule}

Copyright (c) 2007-2009 Intel Corporation. All Rights Reserved.

Permission is hereby granted, free of charge, to any person obtaining a
copy of this software and associated documentation files (the
"Software"), to deal in the Software without restriction, including
without limitation the rights to use, copy, modify, merge, publish,
distribute, sub license, and/or sell copies of the Software, and to
permit persons to whom the Software is furnished to do so, subject to
the following conditions:

The above copyright notice and this permission notice (including the
next paragraph) shall be included in all copies or substantial portions
of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NON-INFRINGEMENT.
IN NO EVENT SHALL INTEL AND/OR ITS SUPPLIERS BE LIABLE FOR
ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
`;

// The desktop server (bin/desktop-server[.exe], Windows and Linux packages). Licenses
// with identical text are printed once, listing the packages that use them.
let serverText = '';
try {
  // Crates for either platform, each listed once.
  const byId = new Map();
  const targets = ['x86_64-pc-windows-msvc', 'x86_64-unknown-linux-gnu', 'aarch64-apple-darwin', 'x86_64-apple-darwin'];
  for (const d of targets.flatMap((t) => serverDependencies(t))) {
    byId.set(`${d.name} ${d.version}`, d);
  }
  const deps = [...byId.values()].sort((a, b) => a.name.localeCompare(b.name));
  const groups = new Map(); // license text -> [package labels]
  for (const d of deps) {
    for (const t of d.texts) {
      const key = t.text.replace(/\r\n/g, '\n');
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key).push(`${d.name} ${d.version} (${t.file})`);
    }
  }
  serverText = `
${rule}
desktop-server (Windows, Linux and macOS packages, bin/): Rust crates compiled into it
${rule}

${deps.map((d) => `  ${d.name} ${d.version} — ${d.license}${d.repository ? ` — ${d.repository}` : ''}`).join('\n')}

libopus (inside opusic-sys) is BSD-3-Clause; its license is among the texts below.
H.264 encoding uses the encoders that come with Windows (Media Foundation);
no H.264 encoder is included.
` + [...groups].map(([body, users]) => `
${rule}
${users.join('\n')}
${rule}

${body}
`).join('');
} catch (e) {
  console.warn(`server notices skipped (${e.message.split('\n')[0]}); is Rust installed?`);
}

// Cisco's OpenH264 binary: not bundled, but its license conditions require this
// text wherever licensing information is shown to users.
const openh264Text = `
${rule}
OpenH264 (Linux, optional; downloaded from Cisco to the remote machine when the
user turns on the software encoder; not included in this package)
OpenH264 Video Codec provided by Cisco Systems, Inc.
${rule}

${fs.readFileSync('scripts/openh264-binary-license.txt', 'utf8').trim()}
`;

fs.writeFileSync('ThirdPartyNotices.txt', text + nvencText + vaText + openh264Text + serverText);
console.log(`ThirdPartyNotices.txt written (opus-decoder ${opusDecoderVersion}${serverText ? ', desktop server' : ''})`);
