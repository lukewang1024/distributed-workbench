#!/usr/bin/env node
// Use a node-local Node/npm installation; never download or copy Node here.
import { spawnSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdirSync, readFileSync, writeFileSync, realpathSync } from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';
import os from 'node:os';
import { fileURLToPath } from 'node:url';
import { packageHost } from './package-computer-use-host.mjs';
import { trimRuntime } from './trim-computer-use-runtime.mjs';
const source = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../computer-use');
const destination = path.resolve(process.argv[2] || source);
const expected = JSON.parse(readFileSync(path.join(source, 'node-runtimes.json'))).version;
if (process.versions.node !== expected) throw new Error(`Node ${expected} is required; install/select it with nodenv, nvm or your system package manager, then retry.`);
const node = realpathSync(process.execPath);
const candidates = [process.env.WORKBENCH_NPM_CLI,
  path.join(path.dirname(node), 'node_modules/npm/bin/npm-cli.js'),
  path.resolve(path.dirname(node), '../lib/node_modules/npm/bin/npm-cli.js')];
try { candidates.push(path.join(path.dirname(createRequire(import.meta.url).resolve('npm/package.json')), 'bin/npm-cli.js')); } catch {}
const npm = candidates.find(p => p && existsSync(p));
if (!npm) throw new Error('npm is required alongside the selected Node; install npm or set WORKBENCH_NPM_CLI to npm-cli.js.');
mkdirSync(destination, { recursive: true });
if (source !== destination) {
  for (const name of ['package.json', 'package-lock.json', 'node-runtimes.json', 'host-version.json', 'extension-host.mjs', 'environment.mjs', 'linux-runtime.mjs', 'release.mjs', 'host.mjs']) copyFileSync(path.join(source, name), path.join(destination, name));
}
const installed = spawnSync(node, [npm, 'ci', '--omit=dev', '--ignore-scripts', '--no-audit', '--no-fund'], {
  cwd:destination, stdio:'inherit', env:{...process.env, npm_config_cache:process.env.npm_config_cache || process.env.NPM_CONFIG_CACHE || path.join(process.env.XDG_CACHE_HOME || (process.platform === 'win32' ? path.join(process.env.LOCALAPPDATA, 'Cache') : path.join(os.homedir(), '.cache')), 'distributed-workbench', 'npm'), PATH:path.dirname(node)+path.delimiter+(process.env.PATH || '')},
});
if (installed.status !== 0) throw new Error(`npm installation failed: ${installed.status ?? installed.error}`);
await trimRuntime(destination);
const { manifest } = await packageHost(destination, JSON.parse(readFileSync(path.join(source, 'host-version.json'))).version);
writeFileSync(path.join(destination, 'host-release.json'), JSON.stringify(manifest));
// Native services must not depend on an interactive nvm shell or nodenv shim.
writeFileSync(path.join(destination, 'node-path'), node+'\n');
console.log(`Installed locked Computer Use dependencies; using local Node ${expected}: ${node}`);
