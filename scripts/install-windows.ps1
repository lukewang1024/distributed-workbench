param(
  [Parameter(Mandatory = $true)][string]$Binary,
  [string]$NodeId = $env:COMPUTERNAME,
  [ValidatePattern('^[0-9A-Za-z._-]+$')][string]$Namespace = "stable",
  [string[]]$AllowRoot = @("C:\Users", "C:\ProgramData\distributed-workbench")
)

$ErrorActionPreference = "Stop"
$suffix = if ($Namespace -eq "stable") { "" } else { "-" + $Namespace }
$installRoot = Join-Path $env:ProgramFiles ("distributed-workbench" + $suffix)
$stateRoot = Join-Path $env:ProgramData ("distributed-workbench" + $suffix)
$installedBinary = Join-Path $installRoot "workbench.exe"
$controllerSocket = Join-Path $stateRoot "controller.sock"
$executorSocket = Join-Path $stateRoot "executor.sock"
$controllerState = Join-Path $stateRoot "controller.json"
$executorState = Join-Path $stateRoot "executor-fences.json"
$backupRoot = Join-Path $stateRoot ("backups\" + (Get-Date).ToUniversalTime().ToString("yyyyMMddTHHmmssZ"))

New-Item -ItemType Directory -Force -Path $installRoot, $stateRoot | Out-Null
if ((Test-Path $controllerState) -or (Test-Path $executorState)) {
  New-Item -ItemType Directory -Force -Path $backupRoot | Out-Null
  foreach ($stateFile in @($controllerState, $executorState)) {
    if (Test-Path $stateFile) { Copy-Item -LiteralPath $stateFile -Destination $backupRoot }
  }
}
$fabricRoot = Join-Path $stateRoot "fabric"
New-Item -ItemType Directory -Force -Path $fabricRoot | Out-Null
# peer accept runs as the authenticated OpenSSH user and creates only proxy
# endpoints in this directory. Controller state remains writable by services.
& icacls.exe $fabricRoot /grant '*S-1-5-11:(OI)(CI)M' /T /C | Out-Null
if ($LASTEXITCODE -ne 0) { throw "failed to grant fabric IPC directory access" }
$serviceNamespace = if ($Namespace -eq "stable") { "" } else { (Get-Culture).TextInfo.ToTitleCase($Namespace) }
$controllerService = "DistributedWorkbench" + $serviceNamespace + "Controller"
$executorService = "DistributedWorkbench" + $serviceNamespace + "Executor"

# Upgrades must not silently narrow an executor's filesystem grants. Callers
# may omit machine-specific roots that were supplied during initial bootstrap
# (for example a data drive backing a junction below C:\Users). Preserve those
# roots from the existing managed service command line and merge them with the
# newly requested set.
$effectiveAllowRoots = [System.Collections.Generic.List[string]]::new()
function Add-AllowRoot([string]$Root) {
  if ([string]::IsNullOrWhiteSpace($Root)) { return }
  if (-not [System.IO.Path]::IsPathRooted($Root)) {
    throw "allow-root must be absolute: $Root"
  }
  if (-not ($effectiveAllowRoots | Where-Object { $_.Equals($Root, [System.StringComparison]::OrdinalIgnoreCase) })) {
    $effectiveAllowRoots.Add($Root)
  }
}
foreach ($root in $AllowRoot) { Add-AllowRoot $root }
$existingExecutor = Get-CimInstance Win32_Service -Filter "Name='$executorService'" -ErrorAction SilentlyContinue
if ($existingExecutor -and $existingExecutor.PathName) {
  foreach ($match in [regex]::Matches($existingExecutor.PathName, '(?i)--allow-root\s+"([^"]+)"')) {
    Add-AllowRoot $match.Groups[1].Value
  }
}

foreach ($serviceName in @($controllerService, $executorService)) {
  if (Get-Service -Name $serviceName -ErrorAction SilentlyContinue) {
    Stop-Service -Name $serviceName -Force -ErrorAction SilentlyContinue
  }
}
$stopDeadline = (Get-Date).AddSeconds(10)
do {
  $running = @(Get-Process -Name "workbench" -ErrorAction SilentlyContinue | Where-Object {
      $_.Path -eq $installedBinary
    })
  if ($running.Count -eq 0) { break }
  $running | Stop-Process -Force -ErrorAction SilentlyContinue
  if ((Get-Date) -ge $stopDeadline) {
    throw "timed out waiting for existing workbench processes to exit"
  }
  Start-Sleep -Milliseconds 200
} while ($true)
Copy-Item -Force -LiteralPath $Binary -Destination $installedBinary

function Quote-Arg([string]$Value) {
  return '"' + $Value.Replace('"', '\"') + '"'
}

function Invoke-Sc([string]$Line) {
  $line = 'sc.exe ' + $Line
  $process = Start-Process -FilePath $env:ComSpec -ArgumentList @('/d', '/c', $line) -Wait -NoNewWindow -PassThru
  if ($process.ExitCode -ne 0) {
    throw "sc.exe failed with exit code $($process.ExitCode): $line"
  }
}

$controllerArgs = @(
  (Quote-Arg $installedBinary)
  "--socket", (Quote-Arg $controllerSocket)
  "controller", "serve", "--state", (Quote-Arg $controllerState), "--id", (Quote-Arg $NodeId)
) -join " "
$executorParts = @(
  (Quote-Arg $installedBinary)
  "--socket", (Quote-Arg $executorSocket)
  "executor", "serve", "--id", (Quote-Arg ($NodeId + "-native"))
  "--state", (Quote-Arg $executorState)
)
Add-AllowRoot $stateRoot
foreach ($root in $effectiveAllowRoots) {
  $executorParts += @("--allow-root", (Quote-Arg $root))
}
$executorArgs = $executorParts -join " "

foreach ($service in @(
  @{ Name = $controllerService; Display = "Distributed Workbench $Namespace Controller"; Command = $controllerArgs },
  @{ Name = $executorService; Display = "Distributed Workbench $Namespace Executor"; Command = $executorArgs }
)) {
  $existing = Get-Service -Name $service.Name -ErrorAction SilentlyContinue
  $scCommand = $service.Command.Replace('"', '\"')
  if ($existing) {
    Stop-Service -Name $service.Name -Force -ErrorAction SilentlyContinue
    Invoke-Sc ('config ' + $service.Name + ' binPath= "' + $scCommand + '" start= auto')
  } else {
    Invoke-Sc ('create ' + $service.Name + ' binPath= "' + $scCommand + '" start= auto DisplayName= "' + $service.Display + '"')
  }
  Invoke-Sc ('failure ' + $service.Name + ' reset= 86400 actions= restart/1000/restart/5000/restart/30000')
  Start-Service -Name $service.Name
}

function Test-LocalSocketReady([string]$Socket) {
  try {
    & $installedBinary --socket $Socket status 2>$null | Out-Null
    return $LASTEXITCODE -eq 0
  } catch {
    return $false
  }
}

$deadline = (Get-Date).AddSeconds(20)
do {
  $controllerReady = Test-LocalSocketReady $controllerSocket
  $executorReady = Test-LocalSocketReady $executorSocket
  if ($controllerReady -and $executorReady) {
    break
  }
  if ((Get-Date) -ge $deadline) { throw "services did not become ready" }
  Start-Sleep -Milliseconds 200
} while ($true)

$registrationParams = @{
  executorId = $NodeId + "-native"
  endpoint = @{ transport = "local"; socket = $executorSocket }
}
$registrationRequest = @{
  apiVersion = "workbench.dev/v1"
  requestId = "req_install_windows"
  action = "executor.register"
  params = $registrationParams
} | ConvertTo-Json -Compress -Depth 8
$registrationInfo = New-Object System.Diagnostics.ProcessStartInfo
$registrationInfo.FileName = $installedBinary
$registrationInfo.Arguments = '--socket "' + $controllerSocket + '" call-stdin'
$registrationInfo.UseShellExecute = $false
$registrationInfo.RedirectStandardInput = $true
$registrationInfo.RedirectStandardOutput = $true
$registrationInfo.RedirectStandardError = $true
$registrationProcess = [Diagnostics.Process]::Start($registrationInfo)
$registrationProcess.StandardInput.WriteLine($registrationRequest)
$registrationProcess.StandardInput.Close()
$registrationStdout = $registrationProcess.StandardOutput.ReadToEnd()
$registrationStderr = $registrationProcess.StandardError.ReadToEnd()
$registrationProcess.WaitForExit()
[Console]::Out.Write($registrationStdout)
[Console]::Error.Write($registrationStderr)
if ($registrationProcess.ExitCode -ne 0) { throw "failed to register local executor" }
Write-Output $installedBinary
