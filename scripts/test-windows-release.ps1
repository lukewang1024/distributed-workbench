param([Parameter(Mandatory = $true)][string]$Archive)
$ErrorActionPreference = "Stop"
$temporary = Join-Path $env:TEMP ("distributed-workbench-long-path-install-regression-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $temporary | Out-Null
try {
  & tar.exe -xf $Archive -C $temporary
  if ($LASTEXITCODE -ne 0) { throw 'release extraction failed' }
  $package = @(Get-ChildItem -LiteralPath $temporary -Directory)[0].FullName
  $source = Join-Path $package 'computer-use'
  $install = Join-Path $temporary 'Program Files/distributed-workbench'
  $state = Join-Path $temporary 'ProgramData/distributed-workbench'
  $installer = Join-Path $package 'scripts/install-computer-use-runtime.ps1'
  & $installer -ComputerUseSource $source -InstallRoot $install -StateRoot $state
  & $installer -ComputerUseSource $source -InstallRoot $install -StateRoot $state
  $runtime = [IO.File]::ReadAllText((Join-Path $state 'computer-use/runtime-root'))
  # Verify every file, including paths that exceed MAX_PATH, with the bundled Node.
  $check = @'
import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import assert from 'node:assert/strict';
const [source, runtime, state] = process.argv.slice(1);
let longPaths = 0;
function compare(relative = '') {
  for (const entry of readdirSync(join(source, relative), {withFileTypes:true})) {
    const item = join(relative, entry.name);
    if (entry.isDirectory()) compare(item);
    else {
      assert.deepEqual(readFileSync(join(source,item)), readFileSync(join(runtime,item)), item);
      if (join(runtime,item).length > 260) longPaths++;
    }
  }
}
compare();
assert(longPaths > 0, 'fixture must exercise long paths');
const { loadHost } = await import(pathToFileURL(join(runtime,'extension-host.mjs')));
const host = await loadHost(state);
assert.equal(host.tools().length,11);
await host.close();
console.log(`Installed original plugin with ${longPaths} long paths; all files match and all 11 tools load.`);
'@
  & (Join-Path $runtime 'node.exe') --input-type=module -e $check $source $runtime (Join-Path $state 'probe')
  if ($LASTEXITCODE -ne 0) { throw 'installed plugin validation failed' }
} finally {
  & node -e "require('node:fs').rmSync(process.argv[1],{recursive:true,force:true})" $temporary
}
