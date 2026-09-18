param(
  [Parameter(Mandatory = $true)][string]$ComputerUseSource,
  [Parameter(Mandatory = $true)][string]$InstallRoot,
  [Parameter(Mandatory = $true)][string]$StateRoot
)
$ErrorActionPreference = "Stop"
# Keep the original npm plugin beside the managed application data, not staging.

if (Test-Path (Join-Path $computerUseSource 'host.mjs')) {
  $node = $env:WORKBENCH_NODE
  if (!$node) { $command = Get-Command node.exe -ErrorAction SilentlyContinue; if ($command) { $node = $command.Source } }
  $legacyNode = Join-Path $computerUseSource 'node.exe'
  if (Test-Path $legacyNode) { $node = $legacyNode }
  if (!$node) { throw 'Install the locked Node version with a version manager or system package manager, then retry with WORKBENCH_NODE if needed.' }
  $parts = @('host.mjs', 'extension-host.mjs', 'environment.mjs', 'linux-runtime.mjs', 'release.mjs', 'package.json', 'package-lock.json', 'node-runtimes.json') | ForEach-Object {
    (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $computerUseSource $_)).Hash
  }
  if (Test-Path (Join-Path $computerUseSource 'host-version.json')) {
    $parts += (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $computerUseSource 'host-version.json')).Hash
  }
  # Keep the directory fixed-length as host modules are added (Windows path limits).
  $hasher = [Security.Cryptography.SHA256]::Create()
  try {
    $digest = ([BitConverter]::ToString($hasher.ComputeHash([Text.Encoding]::UTF8.GetBytes(($parts -join ''))))).Replace('-', '').ToLowerInvariant()
  } finally { $hasher.Dispose() }
  $computerUseRoot = Join-Path $installRoot ('computer-use/' + $digest)
  if (!(Test-Path $computerUseRoot)) {
    New-Item -ItemType Directory -Force -Path (Split-Path $computerUseRoot) | Out-Null
    $computerUseTemporary = $computerUseRoot + '.' + [guid]::NewGuid().ToString('N') + '.tmp'
    try {
      # Robocopy preserves long npm dependency paths on Windows PowerShell 5.1.
      & robocopy.exe $computerUseSource $computerUseTemporary /E /COPY:DAT /DCOPY:DAT /R:1 /W:1 /NFL /NDL /NJH /NJS | Out-Null
      if ($LASTEXITCODE -ge 8) { throw "Computer Use runtime copy failed: $LASTEXITCODE" }
      if (!(Test-Path $legacyNode)) {
        & $node (Join-Path (Split-Path $computerUseSource) 'scripts/install-computer-use.mjs') $computerUseTemporary
        if ($LASTEXITCODE -ne 0) { throw 'Computer Use dependency installation failed' }
      }
      Move-Item -LiteralPath $computerUseTemporary -Destination $computerUseRoot
    } finally {
      if (Test-Path $computerUseTemporary) { Remove-Item -LiteralPath ("\\?\" + $computerUseTemporary) -Recurse -Force }
    }
  }
  $computerUseState = Join-Path $stateRoot 'computer-use'
  New-Item -ItemType Directory -Force -Path $computerUseState | Out-Null
  [IO.File]::WriteAllText((Join-Path $computerUseState 'runtime-root'), $computerUseRoot, (New-Object Text.UTF8Encoding($false)))
}

