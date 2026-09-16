param(
  [Parameter(Mandatory = $true)][string]$ComputerUseSource,
  [Parameter(Mandatory = $true)][string]$InstallRoot,
  [Parameter(Mandatory = $true)][string]$StateRoot
)
$ErrorActionPreference = "Stop"
# Keep the original npm plugin beside the managed application data, not staging.

if (Test-Path (Join-Path $computerUseSource 'host.mjs')) {
  if (!(Test-Path (Join-Path $computerUseSource 'node.exe'))) { throw 'packaged Computer Use Node is missing' }
  $parts = @('host.mjs', 'extension-host.mjs', 'environment.mjs', 'package-lock.json', 'node-runtimes.json') | ForEach-Object {
    (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $computerUseSource $_)).Hash.Substring(0, 16)
  }
  $computerUseRoot = Join-Path $installRoot ('computer-use/' + ($parts -join ''))
  if (!(Test-Path $computerUseRoot)) {
    New-Item -ItemType Directory -Force -Path (Split-Path $computerUseRoot) | Out-Null
    $computerUseTemporary = $computerUseRoot + '.' + [guid]::NewGuid().ToString('N') + '.tmp'
    try {
      # Robocopy preserves long npm dependency paths on Windows PowerShell 5.1.
      & robocopy.exe $computerUseSource $computerUseTemporary /E /COPY:DAT /DCOPY:DAT /R:1 /W:1 /NFL /NDL /NJH /NJS | Out-Null
      if ($LASTEXITCODE -ge 8) { throw "Computer Use runtime copy failed: $LASTEXITCODE" }
      Move-Item -LiteralPath $computerUseTemporary -Destination $computerUseRoot
    } finally {
      if (Test-Path $computerUseTemporary) { Remove-Item -LiteralPath ("\\?\" + $computerUseTemporary) -Recurse -Force }
    }
  }
  $computerUseState = Join-Path $stateRoot 'computer-use'
  New-Item -ItemType Directory -Force -Path $computerUseState | Out-Null
  [IO.File]::WriteAllText((Join-Path $computerUseState 'runtime-root'), $computerUseRoot, (New-Object Text.UTF8Encoding($false)))
}

