#!/usr/bin/env node
// Platform-neutral host artifact: no Node, npm packages or native helpers.
import { readFile, writeFile, mkdir } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { hostFiles, sha256 } from '../computer-use/release.mjs';
const source = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../computer-use');
export async function packageHost(destination, version) {
  if (!/^\d+\.\d+\.\d+$/.test(version)) throw new Error('Host version must be exact semver');
  const files = {};
  const digests = {};
  for (const name of hostFiles) {
    const bytes = await readFile(path.join(source, name));
    files[name] = bytes.toString('base64');
    digests[name] = sha256(bytes);
  }
  const manifest = { component: 'computer-use-host', version, protocol: 1,
    nodeVersion: JSON.parse(await readFile(path.join(source, 'node-runtimes.json'))).version,
    dependencyDigest: sha256(await readFile(path.join(source, 'package-lock.json'))), files: digests };
  await mkdir(destination, { recursive: true });
  const artifact = path.join(destination, `computer-use-host-${version}.json`);
  await writeFile(artifact, JSON.stringify({ manifest, files }) + '\n');
  await writeFile(`${artifact}.sha256`, sha256(await readFile(artifact)) + '\n');
  return { artifact, manifest };
}
if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const version = process.argv[2];
  const destination = path.resolve(process.argv[3] || 'dist');
  console.log(JSON.stringify(await packageHost(destination, version)));
}
