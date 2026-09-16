// Host release identity and dependency compatibility, shared by startup and install.
import { readFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import path from 'node:path';
export const hostFiles = ['host.mjs', 'extension-host.mjs', 'environment.mjs', 'linux-runtime.mjs', 'release.mjs'];
export const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
export const lockDigest = bytes => sha256(JSON.stringify(JSON.parse(bytes.toString())));
export async function validateCompatibility(hostRoot, runtimeRoot) {
  const manifest = JSON.parse(await readFile(path.join(hostRoot, 'host-release.json'), 'utf8'));
  if (manifest.protocol !== 1) throw new Error('Unsupported CU host protocol');
  const dependencyDigest = lockDigest(await readFile(path.join(runtimeRoot, 'package-lock.json')));
  if (manifest.dependencyDigest !== dependencyDigest) throw new Error('CU host dependency lock mismatch');
  if (manifest.nodeVersion !== process.versions.node) throw new Error('CU host Node version mismatch');
  for (const name of hostFiles) {
    if (sha256(await readFile(path.join(hostRoot, name))) !== manifest.files[name]) {
      throw new Error(`CU host integrity mismatch: ${name}`);
    }
  }
  return { component: 'computer-use-host', version: manifest.version, protocol: manifest.protocol,
    dependencyDigest, manifestDigest: sha256(JSON.stringify(manifest)), nodeVersion: process.versions.node };
}
