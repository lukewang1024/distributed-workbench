#!/usr/bin/env node
// Install locked upstream packages and a checksum-pinned official Node runtime.
import { spawnSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdirSync, chmodSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { createHash } from 'node:crypto';
import path from 'node:path';
import os from 'node:os';
import { fileURLToPath } from 'node:url';
import { packageHost } from './package-computer-use-host.mjs';
const source = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../computer-use');
const destination = path.resolve(process.argv[2] || source);
if (Number(process.versions.node.split('.')[0]) < 24) throw new Error('Node >=24 is required');
mkdirSync(destination, { recursive: true });
if (source !== destination) {
  for (const name of ['package.json', 'package-lock.json', 'node-runtimes.json', 'extension-host.mjs', 'environment.mjs', 'linux-runtime.mjs', 'release.mjs', 'host.mjs']) copyFileSync(path.join(source, name), path.join(destination, name));
}
const manifest = JSON.parse(readFileSync(path.join(source, 'node-runtimes.json')));
const entry = manifest.archives[`${process.platform}-${process.arch}`];
if (!entry) throw new Error(`Unsupported desktop platform: ${process.platform}-${process.arch}`);
const [archiveName, expected] = entry;
const cache = path.join(process.env.XDG_CACHE_HOME || (process.platform === 'win32'
  ? path.join(process.env.LOCALAPPDATA, 'Cache') : path.join(os.homedir(), '.cache')), 'distributed-workbench', 'computer-use');
mkdirSync(cache, { recursive: true });
const archive = path.join(cache, archiveName);
if (!existsSync(archive)) {
  const response = await fetch(`https://nodejs.org/dist/v${manifest.version}/${archiveName}`);
  if (!response.ok) throw new Error(`Node download failed: ${response.status}`);
  writeFileSync(archive, Buffer.from(await response.arrayBuffer()));
}
if (createHash('sha256').update(readFileSync(archive)).digest('hex') !== expected) throw new Error('Node archive SHA-256 mismatch');
const extracted = path.join(cache, archiveName.replace(/\.(tar\.gz|zip)$/, ''));
if (!existsSync(extracted)) {
  const extraction = spawnSync('tar', ['-xf', archive, '-C', cache], { stdio:'inherit' });
  if (extraction.status !== 0) throw new Error('Node archive extraction failed');
}
const node = path.join(destination, process.platform === 'win32' ? 'node.exe' : 'node');
// Replace the old file first: never truncate an executable shared with a running host.
if (existsSync(node)) rmSync(node);
copyFileSync(path.join(extracted, process.platform === 'win32' ? 'node.exe' : 'bin/node'), node);
if (process.platform !== 'win32') chmodSync(node, 0o755);
const npm = path.join(extracted, process.platform === 'win32' ? 'node_modules/npm/bin/npm-cli.js' : 'lib/node_modules/npm/bin/npm-cli.js');
const installed = spawnSync(node, [npm, 'ci', '--omit=dev', '--ignore-scripts', '--no-audit', '--no-fund'], { cwd:destination, stdio:'inherit' });
if (installed.status !== 0) process.exit(installed.status || 1);
copyFileSync(path.join(extracted, 'LICENSE'), path.join(destination, 'NODE-LICENSE'));
console.log(`Installed locked Pi computer-use host and Node ${manifest.version} at ${destination}`);
console.log('Native helpers remain upstream-owned; OS permissions may be required on first use.');

const { manifest: hostManifest } = await packageHost(cache, JSON.parse(readFileSync(path.join(source, 'host-version.json'))).version);
writeFileSync(path.join(destination, 'host-release.json'), JSON.stringify(hostManifest));
