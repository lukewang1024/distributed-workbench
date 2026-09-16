import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { configureEnvironment } from '../environment.mjs';

test('CU child overrides graphical session without changing parent clipboard environment', async () => {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'cu-env-'));
  try {
    const desired = {DISPLAY: ':1', XAUTHORITY: '/home/wangyuanlv/.Xauthority',
      DBUS_SESSION_BUS_ADDRESS: 'unix:path=/run/user/1001/bus', PI_COMPUTER_USE_HEADLESS: 'false'};
    await writeFile(path.join(dir, 'environment.json'), JSON.stringify(desired));
    const parent = {...process.env, DISPLAY: ':97', XAUTHORITY: '/clipboard-Xauthority'};
    const script = `import {configureEnvironment} from ${JSON.stringify(new URL('../environment.mjs', import.meta.url).href)}; await configureEnvironment(process.argv[1]); console.log(JSON.stringify(Object.fromEntries(${JSON.stringify(Object.keys(desired))}.map(k=>[k,process.env[k]]))));`;
    const child = spawnSync(process.execPath, ['--input-type=module', '-e', script, dir], {env:parent, encoding:'utf8'});
    assert.equal(child.status, 0, child.stderr);
    assert.deepEqual(JSON.parse(child.stdout), desired);
    assert.equal(parent.DISPLAY, ':97');
    assert.equal(parent.XAUTHORITY, '/clipboard-Xauthority');
  } finally { await rm(dir, {recursive:true, force:true}); }
});

test('missing config inherits environment, invalid config fails atomically, new host reads updates', async () => {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'cu-env-'));
  const env = {DISPLAY: ':97'};
  try {
    await configureEnvironment(dir, env);
    assert.deepEqual(env, {DISPLAY: ':97'});
    for (const value of [[], null, {DISPLAY:':1', PATH:'/bad'}, {DISPLAY:1}, {DISPLAY:'a\0b'}, {PI_COMPUTER_USE_HEADLESS:'maybe'}]) {
      await writeFile(path.join(dir, 'environment.json'), JSON.stringify(value));
      await assert.rejects(configureEnvironment(dir, env));
      assert.deepEqual(env, {DISPLAY: ':97'});
    }
    await writeFile(path.join(dir, 'environment.json'), '{broken');
    await assert.rejects(configureEnvironment(dir, env));
    await writeFile(path.join(dir, 'environment.json'), '{"DISPLAY":":1"}');
    await configureEnvironment(dir, env);
    assert.equal(env.DISPLAY, ':1');
    await writeFile(path.join(dir, 'environment.json'), '{"DISPLAY":":2"}');
    await configureEnvironment(dir, env);
    assert.equal(env.DISPLAY, ':2');
  } finally { await rm(dir, {recursive:true, force:true}); }
});
