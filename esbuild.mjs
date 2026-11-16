// Builds two bundles:
//  - dist/extension.js : the extension host side (Node, runs on the remote machine)
//  - dist/webview.js   : the viewer that runs inside the editor tab (browser, runs on your machine)
import * as esbuild from 'esbuild';

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
    entryPoints: ['src/webview/main.ts'],
    outfile: 'dist/webview.js',
    bundle: true,
    platform: 'browser',
    // ES module (loaded with <script type="module">) because noVNC uses top-level await.
    format: 'esm',
    target: 'es2022',
    sourcemap: true,
  },
  {
    // Exposed separately so it can be run/tested without VS Code.
    entryPoints: ['src/testServerCli.ts'],
    outfile: 'dist/testServer.js',
    bundle: true,
    platform: 'node',
    format: 'cjs',
    target: 'node18',
  },
];

if (watch) {
  for (const c of configs) (await esbuild.context(c)).watch();
  console.log('watching...');
} else {
  await Promise.all(configs.map((c) => esbuild.build(c)));
  console.log('built');
}
