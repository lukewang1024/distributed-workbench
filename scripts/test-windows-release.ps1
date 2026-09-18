param([Parameter(Mandatory = $true)][string]$Archive)
$ErrorActionPreference = "Stop"
$cleanupNode = $env:WORKBENCH_NODE
if (!$cleanupNode) { $cleanupNode = (Get-Command node.exe -ErrorAction Stop).Source }
$temporary = Join-Path $env:TEMP ("distributed-workbench-long-path-install-regression-" + [guid]::NewGuid())
# Keep the final runtime root comparable to Program Files on production nodes.
# Node package imports can fail under an artificially elongated installation root,
# independently of the archive/copy APIs being tested here.
$installedFixture = Join-Path $env:TEMP ("dwb-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $temporary, $installedFixture | Out-Null
try {
  & tar.exe -xf $Archive -C $temporary
  if ($LASTEXITCODE -ne 0) { throw 'release extraction failed' }
  $package = @(Get-ChildItem -LiteralPath $temporary -Directory)[0].FullName
  $source = Join-Path $package 'computer-use'
  $install = Join-Path $installedFixture 'install'
  $state = Join-Path $installedFixture 'state'
  $installer = Join-Path $package 'scripts/install-computer-use-runtime.ps1'
  & $installer -ComputerUseSource $source -InstallRoot $install -StateRoot $state
  $selection = Join-Path $state 'computer-use/runtime-root'
  $runtime = [IO.File]::ReadAllText($selection)
  $offline = $env:npm_config_offline
  try {
    $env:npm_config_offline = 'true'
    & $installer -ComputerUseSource $source -InstallRoot $install -StateRoot $state
    if ([IO.File]::ReadAllText($selection) -ne $runtime) { throw 'repeat install changed runtime' }
  } finally { $env:npm_config_offline = $offline }
  $previousNode = $env:WORKBENCH_NODE
  $previousPath = $env:PATH
  try {
    $env:WORKBENCH_NODE = $null
    $env:PATH = Join-Path $env:SystemRoot 'System32'
    $rejected = $false
    try { & $installer -ComputerUseSource $source -InstallRoot $install -StateRoot $state }
    catch { if ($_.Exception.Message -notmatch 'Install the locked Node version') { throw }; $rejected = $true }
    if (!$rejected) { throw 'missing Node was accepted' }
  } finally { $env:WORKBENCH_NODE = $previousNode; $env:PATH = $previousPath }
  $lock = Join-Path $source 'node-runtimes.json'
  $originalLock = [IO.File]::ReadAllText($lock)
  try {
    $invalidLock = $originalLock | ConvertFrom-Json
    $invalidLock.version = '0.0.0'
    [IO.File]::WriteAllText($lock, ($invalidLock | ConvertTo-Json -Depth 10))
    $rejected = $false
    try { & $installer -ComputerUseSource $source -InstallRoot $install -StateRoot $state }
    catch { $rejected = $true }
    if (!$rejected) { throw 'incompatible Node was accepted' }
    if ([IO.File]::ReadAllText($selection) -ne $runtime) { throw 'failed install changed runtime' }
    if (Get-ChildItem (Join-Path $install 'computer-use') -Filter '*.tmp') { throw 'failed install left staging behind' }
  } finally { [IO.File]::WriteAllText($lock, $originalLock) }
  Write-Output 'PASS: offline reuse, missing Node, version mismatch, preserved selection and staging cleanup.'
  # Verify scripts and locally installed dependencies using selected node-local Node.
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
      if (join(source,item).length > 260 || join(runtime,item).length > 260) longPaths++;
    }
  }
}
compare();
assert(!readdirSync(source).includes('node.exe'));
assert(!readdirSync(source).includes('node_modules'));
assert(readdirSync(join(runtime,'node_modules')).length > 0);
assert(!readdirSync(runtime).includes('node.exe'));
assert.equal(readFileSync(join(runtime,'node-path'),'utf8').trim().toLowerCase(), process.execPath.toLowerCase());
const { validateCompatibility } = await import(pathToFileURL(join(runtime,'release.mjs')));
await validateCompatibility(runtime, runtime);
const { loadHost } = await import(pathToFileURL(join(runtime,'extension-host.mjs')));
const host = await loadHost(state);
assert.equal(host.tools().length,11);
await host.close();
console.log(`Installed original plugin with ${longPaths} long paths; all files match and all 11 tools load.`);
'@
  $nodeExe = [IO.File]::ReadAllText((Join-Path $runtime 'node-path')).Trim()
  & $nodeExe --input-type=module -e $check $source $runtime (Join-Path $state 'probe')
  if ($LASTEXITCODE -ne 0) { throw 'installed plugin validation failed' }
} finally {
  & $cleanupNode -e "for(const p of process.argv.slice(1)) require('node:fs').rmSync(p,{recursive:true,force:true})" $temporary $installedFixture
}
