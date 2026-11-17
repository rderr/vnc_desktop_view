// Builds two bundles:
//  - dist/extension.js : the extension host side (Node, runs on the remote machine)
//  - dist/video.js     : the viewer that runs inside the editor tab (browser, runs on your machine)
import * as esbuild from 'esbuild';
import * as fs from 'fs';

const watch = process.argv.includes('--watch');

const configs = [
  {
    entryPoints: ['src/extension.ts'],
    outfile: 'dist/extension.js',
    bundle: true,
    platform: 'node',
    format: 'cjs',
    target: 'node18',
    external: ['vscode'],
    sourcemap: true,
  },
  {
    entryPoints: ['src/webview/video.ts'],
    outfile: 'dist/video.js',
    bundle: true,
    platform: 'browser',
    format: 'iife',
    target: 'es2022',
    sourcemap: true,
  },
];

if (watch) {
  for (const c of configs) (await esbuild.context(c)).watch();
  console.log('watching...');
} else {
  fs.rmSync('dist', { recursive: true, force: true }); // no stale bundles in the package
  await Promise.all(configs.map((c) => esbuild.build(c)));
  console.log('built');
}
