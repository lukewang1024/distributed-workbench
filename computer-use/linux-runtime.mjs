// Process launch compatibility only; upstream helper bytes and UI logic stay intact.
import childProcess from 'node:child_process';
import { syncBuiltinESMExports } from 'node:module';
import { access, readFile, stat } from 'node:fs/promises';
import { constants } from 'node:fs';
import path from 'node:path';

export async function configureLinuxRuntime(stateDir, helperPath) {
  if (process.platform !== 'linux') return () => {};
  let config;
  try {
    config = JSON.parse(await readFile(path.join(stateDir, 'linux-runtime.json'), 'utf8'));
  } catch (error) {
    if (error.code === 'ENOENT') return () => {};
    throw error;
  }
  if (!config || Array.isArray(config) || typeof config !== 'object'
      || Object.keys(config).some(key => !['loader', 'libraryPath'].includes(key))
      || typeof config.loader !== 'string' || !path.isAbsolute(config.loader)
      || config.loader.includes('\0') || !Array.isArray(config.libraryPath)
      || !config.libraryPath.length || config.libraryPath.some(dir =>
        typeof dir !== 'string' || !path.isAbsolute(dir) || /[:\0]/.test(dir))) {
    throw new Error('linux-runtime.json requires an absolute loader and nonempty absolute libraryPath array');
  }
  await access(config.loader, constants.X_OK);
  if (!(await stat(config.loader)).isFile()) throw new Error('CU loader must be an executable file');
  for (const dir of config.libraryPath) {
    if (!(await stat(dir)).isDirectory()) throw new Error('CU libraryPath must contain directories');
  }
  const target = path.resolve(helperPath);
  const original = childProcess.spawn;
  function spawn(command, args, options) {
    // The pinned plugin uses this exact absolute path. Never intercept Node,
    // setup-helper, shell commands, or unrelated child processes.
    if (command === target) {
      if (!Array.isArray(args) || options?.shell) throw new Error('CU helper launch must use direct argv');
      return original(config.loader, ['--library-path', config.libraryPath.join(':'), target, ...args], options);
    }
    return original(command, args, options);
  }
  childProcess.spawn = spawn;
  syncBuiltinESMExports();
  return () => {
    if (childProcess.spawn === spawn) {
      childProcess.spawn = original;
      syncBuiltinESMExports();
    }
  };
}
