param(
  [string]$Version = "latest",
  [string]$NodeId = $env:COMPUTERNAME,
  [string[]]$AllowRoot = @("C:\Users", "C:\ProgramData\distributed-workbench")
)

$ErrorActionPreference = "Stop"
$repository = if ($env:DISTRIBUTED_WORKBENCH_REPOSITORY) { $env:DISTRIBUTED_WORKBENCH_REPOSITORY } else { "lukewang1024/distributed-workbench" }
if ($Version -eq "latest") {
  $Version = (Invoke-RestMethod "https://api.github.com/repos/$repository/releases/latest").tag_name
}
$Version = $Version.TrimStart("v")
if ($Version -notmatch '^[0-9A-Za-z._-]+$') { throw "invalid version: $Version" }

$target = "x86_64-pc-windows-msvc"
$archive = "distributed-workbench-$Version-$target.zip"
$base = "https://github.com/$repository/releases/download/v$Version"
$temporary = Join-Path ([System.IO.Path]::GetTempPath()) ("distributed-workbench-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $temporary | Out-Null
try {
  Invoke-WebRequest "$base/$archive" -OutFile (Join-Path $temporary $archive)
  Invoke-WebRequest "$base/SHA256SUMS" -OutFile (Join-Path $temporary "SHA256SUMS")
  $sumLine = Get-Content (Join-Path $temporary "SHA256SUMS") | Where-Object { $_ -match ("  " + [regex]::Escape($archive) + '$') }
  if (-not $sumLine) { throw "checksum missing for $archive" }
  $expected = ($sumLine -split '\s+')[0].ToLowerInvariant()
  $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $temporary $archive)).Hash.ToLowerInvariant()
  if ($actual -ne $expected) { throw "checksum mismatch" }
  Expand-Archive -Path (Join-Path $temporary $archive) -DestinationPath $temporary
  $root = Join-Path $temporary "distributed-workbench-$Version-$target"
  & (Join-Path $root "scripts\install-windows.ps1") -Binary (Join-Path $root "bin\workbench.exe") -NodeId $NodeId -AllowRoot $AllowRoot
} finally {
  Remove-Item -Recurse -Force $temporary -ErrorAction SilentlyContinue
}
