// Standalone test VNC server: node dist/testServer.js [port] [light|stress]
import { startTestServer, type TestMode } from './testServer';

export { startTestServer };

if (require.main === module) {
  const port = Number(process.argv[2] ?? 5901);
  const mode = (process.argv[3] ?? 'light') as TestMode;
  startTestServer(mode, (fps) => fps && console.log(`[test-server] ${fps} fps`), port)
    .then((s) => console.log(`[test-server] listening on 127.0.0.1:${s.port} (${mode} mode)`));
}
