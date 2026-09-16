import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import childProcess, {spawn} from 'node:child_process';
import {configureLinuxRuntime} from '../linux-runtime.mjs';

async function output(command, args, options) {
  const child = spawn(command, args, options);
  let stdout = '', stderr = '';
  child.stdout.on('data', c => stdout += c);
  child.stderr.on('data', c => stderr += c);
  const code = await new Promise((resolve,reject) => {child.on('error',reject);child.on('close',resolve);});
  assert.equal(code, 0, stderr);
  return stdout;
}

test('redirects only the exact helper; preserves argv/env/stdio and restores builtin exports', {skip:process.platform==='win32'}, async () => {
  const dir=await mkdtemp(path.join(os.tmpdir(),'cu-runtime-'));
  const platform=Object.getOwnPropertyDescriptor(process,'platform');
  Object.defineProperty(process,'platform',{value:'linux'});
  const original=childProcess.spawn;
  let restore=()=>{};
  try {
    const helper=path.join(dir,'helper with spaces');
    const loader=path.join(dir,'loader');
    await writeFile(helper,'#!/bin/sh\nprintf "%s\\n" "$CU_TEST_MARKER" "$@"\n',{mode:0o700});
    await writeFile(loader,'#!/bin/sh\n[ "$1" = --library-path ] || exit 12\nshift 2\nprintf "via-loader\\n"\nexec "$@"\n',{mode:0o700});
    const config={loader,libraryPath:[dir]};
    await writeFile(path.join(dir,'linux-runtime.json'),JSON.stringify(config));
    restore=await configureLinuxRuntime(dir,helper);
    const args=['literal $(false)','two words'];
    assert.equal(await output(helper,args,{env:{...process.env,CU_TEST_MARKER:'preserved'},stdio:['ignore','pipe','pipe']}), 'via-loader\npreserved\nliteral $(false)\ntwo words\n');
    assert.equal(await output('/bin/sh',['-c','printf unrelated'],{stdio:['ignore','pipe','pipe']}),'unrelated');
    assert.throws(()=>spawn(helper,[],{shell:true}),/direct argv/);
    restore();
    assert.equal(childProcess.spawn,original);
    assert.equal(spawn,original);
    assert.equal(await output(helper,['plain'],{env:{CU_TEST_MARKER:'original'},stdio:['ignore','pipe','pipe']}),'original\nplain\n');
    // Models upstream setup replacing the installed helper: no wrapper is stored there.
    await writeFile(helper,'#!/bin/sh\nprintf replaced\\n\n',{mode:0o700});
    restore=await configureLinuxRuntime(dir,helper);
    assert.match(await output(helper,[],{stdio:['ignore','pipe','pipe']}),/^via-loader\nreplaced/);
  } finally {restore();Object.defineProperty(process,'platform',platform);await rm(dir,{recursive:true,force:true});}
});

test('missing runtime is a no-op and invalid config never changes spawn', {skip:process.platform==='win32'}, async () => {
  const dir=await mkdtemp(path.join(os.tmpdir(),'cu-runtime-invalid-'));
  const platform=Object.getOwnPropertyDescriptor(process,'platform');
  Object.defineProperty(process,'platform',{value:'linux'});
  const original=childProcess.spawn;
  try {
    (await configureLinuxRuntime(dir,'/unused/helper'))();
    for (const config of [null,[],{loader:'relative',libraryPath:[dir]}, {loader:'/bin/sh',libraryPath:[]}, {loader:'/bin/sh',libraryPath:['relative']}, {loader:'/bin/sh',libraryPath:[dir+':bad']}, {loader:'/missing-loader',libraryPath:[dir]}, {loader:'/bin/sh',libraryPath:[dir],extra:true}]) {
      await writeFile(path.join(dir,'linux-runtime.json'),JSON.stringify(config));
      await assert.rejects(configureLinuxRuntime(dir,'/unused/helper'));
      assert.equal(childProcess.spawn,original);
    }
  } finally {Object.defineProperty(process,'platform',platform);await rm(dir,{recursive:true,force:true});}
});
