import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { loadHost } from '../extension-host.mjs';

test('loads actual upstream extension schemas and lifecycle without a model or desktop', async () => {
  const state = await mkdtemp(path.join(os.tmpdir(), 'workbench-cu-'));
  const host = await loadHost(state);
  try {
    const tools = host.tools();
    assert.equal(tools.length, 11);
    assert(tools.some(t => t.name === 'observe_ui'));
    assert(tools.find(t => t.name === 'act_ui').inputSchema.required.includes('stateId'));
    await assert.rejects(host.call('act_ui', {actions: []}, 'invalid'), /Invalid arguments/);
    await assert.rejects(host.call('command.run', {}, 'invalid'), /Unknown computer-use tool/);
    // An expired state must fail before the extension attempts desktop access.
    await assert.rejects(host.call('search_ui', {stateId: 'expired', text: 'test'}, 'stale'), /unavailable or was evicted/);
    assert.equal(process.env.PI_CODING_AGENT_DIR, path.join(state, 'pi'));
  } finally {
    await host.close();
    await rm(state, {recursive:true, force:true});
  }
});
