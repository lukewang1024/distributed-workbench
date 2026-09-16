#!/usr/bin/env node
// Immutable host-only install. Existing sessions keep their current process.
import { readFile, writeFile, mkdir, rename, rm } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { randomUUID } from 'node:crypto';
import { hostFiles, sha256, validateCompatibility } from '../computer-use/release.mjs';
async function optional(file) {
  try { return await readFile(file, 'utf8'); }
  catch (error) { if (error.code === 'ENOENT') return null; throw error; }
}
async function atomic(file, value) {
  const temporary = `${file}.${randomUUID()}.tmp`;
  try { await writeFile(temporary, value, { mode: 0o600, flag: 'wx' }); await rename(temporary, file); }
  finally { await rm(temporary, { force: true }); }
}
export async function installHost({ artifact, digest, state, data, rollback = false }) {
  state = path.resolve(state); data = path.resolve(data);
  await mkdir(state, { recursive: true });
  const lock = path.join(state, 'host-install.lock');
  await mkdir(lock); // Fail closed on concurrent/stale transactions, never steal a live lock.
  let temporary;
  try {
    const runtime = (await readFile(path.join(state, 'runtime-root'), 'utf8')).trim();
    if (!path.isAbsolute(runtime)) throw new Error('runtime-root must be absolute');
    let selected;
    let expectedManifestDigest;
    if (rollback) {
      selected = (await readFile(path.join(state, 'host-previous'), 'utf8')).trim();
      if (!path.isAbsolute(selected)) throw new Error('rollback host must be absolute');
    } else {
      const bytes = await readFile(artifact);
      if (!/^[a-f0-9]{64}$/.test(digest) || sha256(bytes) !== digest) throw new Error('Host artifact SHA-256 mismatch');
      const bundle = JSON.parse(bytes);
      expectedManifestDigest = sha256(JSON.stringify(bundle.manifest));
      if (bundle.manifest.component !== 'computer-use-host' || !/^\d+\.\d+\.\d+$/.test(bundle.manifest.version)) throw new Error('Invalid host identity');
      if (Object.keys(bundle.files).sort().join() !== [...hostFiles].sort().join()) throw new Error('Invalid host file inventory');
      selected = path.join(data, 'computer-use-hosts', digest);
      await mkdir(path.dirname(selected), { recursive: true });
      temporary = `${selected}.${randomUUID()}.tmp`;
      await mkdir(temporary);
      for (const name of hostFiles) {
        const content = Buffer.from(bundle.files[name], 'base64');
        if (sha256(content) !== bundle.manifest.files[name]) throw new Error(`Host file digest mismatch: ${name}`);
        await writeFile(path.join(temporary, name), content);
      }
      await writeFile(path.join(temporary, 'host-release.json'), JSON.stringify(bundle.manifest));
      await validateCompatibility(temporary, runtime);
      try { await rename(temporary, selected); }
      catch (error) { if (!['EEXIST', 'ENOTEMPTY', 'EPERM'].includes(error.code)) throw error; }
    }
    const identity = await validateCompatibility(selected, runtime);
    if (expectedManifestDigest && identity.manifestDigest !== expectedManifestDigest) throw new Error('Installed host manifest differs from artifact');
    const previous = (await optional(path.join(state, 'host-root')))?.trim() || runtime;
    if (previous !== selected) {
      await atomic(path.join(state, 'host-previous'), previous + '\n');
      await atomic(path.join(state, 'host-root'), selected + '\n');
    }
    return { ...identity, selected, activation: 'next-host-session' };
  } finally {
    if (temporary) await rm(temporary, { recursive: true, force: true });
    await rm(lock, { recursive: true });
  }
}
if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const [artifact, digest, state, data] = process.argv.slice(2);
  if (!state || !data) throw new Error('usage: node install-computer-use-host.mjs ARTIFACT|--rollback SHA256|- CU_STATE DATA_ROOT');
  console.log(JSON.stringify(await installHost({ artifact, digest, state, data, rollback: artifact === '--rollback' })));
}
