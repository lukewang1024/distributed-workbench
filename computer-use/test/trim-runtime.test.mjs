import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, access, rm } from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import { trimRuntime } from '../../scripts/trim-computer-use-runtime.mjs';

test('targeted pruning handles nested platform dependencies and preserves SDK/unknown packages', async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), 'cu-trim-'));
  try {
    for (const prefix of ['node_modules', 'node_modules/pi/node_modules']) {
      for (const [name, platform, arch] of [['linux-x64','linux','x64'],['darwin-arm64','darwin','arm64'],['win32-x64','win32','x64']]) {
        const pkg = path.join(root, prefix, '@esbuild', name);
        await mkdir(pkg, {recursive:true});
        await writeFile(path.join(pkg, 'package.json'), JSON.stringify({name:`@esbuild/${name}`,os:[platform],cpu:[arch]}));
      }
      await mkdir(path.join(root,prefix,'openai'),{recursive:true});
      await mkdir(path.join(root,prefix,'@esbuild','unknown'),{recursive:true});
    }
    const removed = await trimRuntime(root, 'linux', 'x64');
    assert.equal(removed.length, 4);
    for (const prefix of ['node_modules','node_modules/pi/node_modules']) {
      await access(path.join(root,prefix,'@esbuild/linux-x64/package.json'));
      await access(path.join(root,prefix,'@esbuild/unknown'));
      await access(path.join(root,prefix,'openai'));
      await assert.rejects(access(path.join(root,prefix,'@esbuild/win32-x64')));
    }
    assert.deepEqual(await trimRuntime(root, 'linux', 'x64'), []);
    await assert.rejects(trimRuntime(root,'linux','s390x'), /Unsupported/);
  } finally { await rm(root,{recursive:true,force:true}); }
});
