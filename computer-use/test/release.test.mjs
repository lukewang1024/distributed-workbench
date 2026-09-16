import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, readFile, writeFile, copyFile, rm } from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import { pathToFileURL } from 'node:url';
import { packageHost } from '../../scripts/package-computer-use-host.mjs';
import { installHost } from '../../scripts/install-computer-use-host.mjs';
import { sha256, hostFiles } from '../release.mjs';
import { packageRoot } from '../extension-host.mjs';

test('independent host verifies dependencies, switches atomically and rolls back without copying runtime', async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), 'cu-release-'));
  try {
    const state = path.join(root, 'state');
    const runtime = path.join(root, 'runtime');
    await mkdir(state); await mkdir(runtime);
    await writeFile(path.join(runtime, 'package-lock.json'), (await readFile(path.join(packageRoot, 'package-lock.json'), 'utf8')).replace(/\r?\n/g, '\r\n'));
    const first = await packageHost(root, '0.1.0');
    for (const name of hostFiles) await copyFile(path.join(packageRoot, name), path.join(runtime, name));
    first.manifest.nodeVersion = process.versions.node;
    await writeFile(path.join(runtime, 'host-release.json'), JSON.stringify(first.manifest));
    await writeFile(path.join(state, 'runtime-root'), runtime);
    const second = await packageHost(root, '0.1.1');
    const bundle = JSON.parse(await readFile(second.artifact));
    bundle.manifest.nodeVersion = process.versions.node;
    await writeFile(second.artifact, JSON.stringify(bundle));
    const options = {artifact: second.artifact, digest: sha256(await readFile(second.artifact)), state, data: root};
    await assert.rejects(installHost({...options, digest: '0'.repeat(64)}), /SHA-256/);
    const installed = await installHost(options);
    assert.equal(installed.version, '0.1.1');
    assert.equal((await readFile(path.join(state, 'runtime-root'), 'utf8')), runtime);
    // Load the actual pinned extension from the old runtime under a separate host root.
    const module = await import(pathToFileURL(path.join(installed.selected, 'extension-host.mjs')));
    const host = await module.loadHost(path.join(root, 'session'), packageRoot);
    assert.equal(host.tools().length, 11);
    await host.close();
    assert.equal((await installHost({...options, rollback: true})).version, '0.1.0');
    await writeFile(path.join(runtime, 'package-lock.json'), '{}');
    await assert.rejects(installHost(options), /dependency lock mismatch/);
    assert.equal((await readFile(path.join(state, 'host-root'), 'utf8')).trim(), runtime);
    await mkdir(path.join(state, 'host-install.lock'));
    await assert.rejects(installHost(options), /EEXIST/);
  } finally { await rm(root, {recursive:true, force:true}); }
});
