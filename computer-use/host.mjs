// Executor-private JSON-lines transport. No HTTP listener or model credentials.
import net from 'node:net';
import readline from 'node:readline';
import { readFile, unlink } from 'node:fs/promises';
import { loadHost } from './extension-host.mjs';

const [endpoint, handshakeFile, stateDir] = process.argv.slice(2);
if (!/^127\.0\.0\.1:\d+$/.test(endpoint || '') || !handshakeFile || !stateDir) {
  throw new Error('usage: node host.mjs 127.0.0.1:PORT HANDSHAKE_FILE STATE_DIR');
}
const token = await readFile(handshakeFile, 'utf8');
await unlink(handshakeFile);
const socket = net.connect({ host: '127.0.0.1', port: Number(endpoint.split(':')[1]) });
socket.setTimeout(15 * 60_000, () => socket.destroy());
socket.on('error', () => process.exit(1));
socket.on('close', () => process.exit(0)); // also ends native helpers through their stdin EOF
await new Promise(resolve => socket.once('connect', resolve));
socket.write(JSON.stringify({token}) + '\n');
let host;
try {
  host = await loadHost(stateDir);
  for await (const line of readline.createInterface({ input: socket, crlfDelay: Infinity })) {
    let request;
    try {
      if (Buffer.byteLength(line) > 1024 * 1024) throw new Error('Request exceeds 1 MiB');
      request = JSON.parse(line);
      const result = request.method === 'tools' ? host.tools()
        : request.method === 'close' ? await host.close()
        : await host.call(request.method, request.args, request.id, AbortSignal.timeout(90_000));
      socket.write(JSON.stringify({ id: request.id, ok: true, result: result ?? null }) + '\n');
      if (request.method === 'close') break;
    } catch (error) {
      socket.write(JSON.stringify({id: request?.id, ok: false, error: String(error.message || error)}) + '\n');
    }
  }
} catch (error) {
  socket.write(JSON.stringify({ok: false, error: String(error.message || error)}) + '\n');
} finally {
  await host?.close().catch(() => {});
  socket.end();
  setTimeout(() => process.exit(0), 100).unref();
}
