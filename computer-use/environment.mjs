// Loaded only inside the dedicated CU host, before importing the upstream SDK.
import path from 'node:path';
import { readFile } from 'node:fs/promises';

const allowed = new Set(['DISPLAY', 'XAUTHORITY', 'DBUS_SESSION_BUS_ADDRESS',
  'AT_SPI_BUS_ADDRESS', 'XDG_RUNTIME_DIR', 'PI_COMPUTER_USE_HEADLESS']);

export async function configureEnvironment(stateDir, env = process.env) {
  let content;
  try {
    content = await readFile(path.join(stateDir, 'environment.json'), 'utf8');
  } catch (error) {
    if (error.code === 'ENOENT') return;
    throw error;
  }
  const values = JSON.parse(content);
  if (!values || Array.isArray(values) || typeof values !== 'object') {
    throw new Error('CU environment.json must be an object');
  }
  // Validate the entire document before changing the child environment.
  for (const [name, value] of Object.entries(values)) {
    if (!allowed.has(name) || typeof value !== 'string' || value.includes('\0')) {
      throw new Error(`Invalid CU environment entry: ${name}`);
    }
    if (name === 'PI_COMPUTER_USE_HEADLESS' && !['true', 'false'].includes(value)) {
      throw new Error('PI_COMPUTER_USE_HEADLESS must be true or false');
    }
  }
  Object.assign(env, values);
}
