#!/usr/bin/env node
// Prune only known platform-specific esbuild packages; retain runtime SDKs.
import { readdir, readFile, rm } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export async function trimRuntime(root, platform = process.platform, arch = process.arch) {
  if (!['linux-x64', 'linux-arm64', 'darwin-arm64', 'darwin-x64', 'win32-x64'].includes(`${platform}-${arch}`)) {
    throw new Error(`Unsupported runtime platform: ${platform}-${arch}`);
  }
  const removed = [];
  async function walk(directory) {
    for (const item of await readdir(directory, { withFileTypes: true })) {
      if (!item.isDirectory()) continue; // Never follow links outside the runtime.
      const child = path.join(directory, item.name);
      if (item.name === '@esbuild') {
        for (const candidate of await readdir(child, { withFileTypes: true })) {
          if (!candidate.isDirectory()) continue;
          const packageRoot = path.join(child, candidate.name);
          let metadata;
          try { metadata = JSON.parse(await readFile(path.join(packageRoot, 'package.json'), 'utf8')); }
          catch { continue; } // Unknown packages stay; don't guess from the name.
          if (metadata.name !== `@esbuild/${candidate.name}`) continue;
          const excludes = (values, value) => Array.isArray(values) && values.length > 0 &&
            (values.includes(`!${value}`) || (!values.includes(value) && !values.includes('any') && values.every(v => !v.startsWith('!'))));
          if (excludes(metadata.os, platform) || excludes(metadata.cpu, arch)) {
            await rm(packageRoot, { recursive: true });
            removed.push(path.relative(root, packageRoot));
          }
        }
      } else {
        await walk(child);
      }
    }
  }
  await walk(path.join(root, 'node_modules'));
  return removed;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const removed = await trimRuntime(path.resolve(process.argv[2]), process.argv[3], process.argv[4]);
  console.log(JSON.stringify({ removed }));
}
