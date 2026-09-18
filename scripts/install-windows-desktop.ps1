param(
    [Parameter(Mandatory=$true)][ValidatePattern('^[A-Za-z0-9._-]+$')][string]$NodeId,
    [string]$User,
    [Parameter(Mandatory=$true)][string]$RuntimeBase64,
    [switch]$Disable,
    [switch]$VerifyOnly
)
$ErrorActionPreference = 'Stop'
$name = 'DistributedWorkbench-CU-' + $NodeId
$root = Join-Path $env:ProgramData ('distributed-workbench-desktop-' + $NodeId)
$script = Join-Path $root 'console.ps1'
$task = Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue
if ($Disable) {
    if ($task -and $task.Settings.Enabled) {
        if ($VerifyOnly) { throw 'Desktop task is enabled but policy is disabled' }
        Disable-ScheduledTask -TaskName $name | Out-Null
    }
    @{task=$name;configuration='disabled';desktop='unmanaged'} | ConvertTo-Json -Compress
    exit 0
}
if (-not $User) { throw 'A desktop user must be selected explicitly' }
$sid = (New-Object Security.Principal.NTAccount($User)).Translate([Security.Principal.SecurityIdentifier]).Value
$bytes = [Convert]::FromBase64String($RuntimeBase64)
$hash = ([BitConverter]::ToString([Security.Cryptography.SHA256]::Create().ComputeHash($bytes))).Replace('-','').ToLowerInvariant()
$exe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
$arguments = '-NoProfile -NonInteractive -File "' + $script + '" -TargetSid "' + $sid + '"'
function Test-RestrictedAcl($path) {
    $acl = Get-Acl -LiteralPath $path
    if (-not $acl.AreAccessRulesProtected) { return $false }
    $allowed = @('S-1-5-18','S-1-5-32-544')
    if ($acl.GetOwner([Security.Principal.SecurityIdentifier]).Value -notin $allowed) { return $false }
    $entries = @($acl.GetAccessRules($true,$true,[Security.Principal.SecurityIdentifier]))
    if ($entries.Count -ne 2) { return $false }
    foreach ($entry in $entries) {
        if ($entry.IdentityReference.Value -notin $allowed -or $entry.AccessControlType -ne 'Allow' -or $entry.FileSystemRights -ne 'FullControl') { return $false }
    }
    return $true
}
if (-not $VerifyOnly) {
    if ((Test-Path $root) -and ((Get-Item $root).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Desktop directory must not be a reparse point' }
    New-Item -ItemType Directory -Force -Path $root | Out-Null
    $acl = New-Object Security.AccessControl.DirectorySecurity
    $acl.SetAccessRuleProtection($true,$false)
    $acl.SetOwner((New-Object Security.Principal.SecurityIdentifier('S-1-5-32-544')))
    foreach ($id in @('S-1-5-18','S-1-5-32-544')) {
        $identity = New-Object Security.Principal.SecurityIdentifier($id)
        $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule($identity,'FullControl','ContainerInherit,ObjectInherit','None','Allow')))
    }
    Set-Acl -LiteralPath $root -AclObject $acl
    # Never write through a pre-existing script link or retain a permissive file ACL.
    if ((Test-Path $script) -and ((Get-Item $script).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Desktop script must not be a reparse point' }
    $temporary = Join-Path $root ([guid]::NewGuid().ToString('N') + '.ps1')
    [IO.File]::WriteAllBytes($temporary,$bytes)
    $tokens=$null; $errors=$null
    [void][Management.Automation.Language.Parser]::ParseFile($temporary,[ref]$tokens,[ref]$errors)
    if ($errors.Count) { throw 'Invalid desktop runtime syntax' }
    Move-Item -Force -LiteralPath $temporary -Destination $script
    $fileAcl = New-Object Security.AccessControl.FileSecurity
    $fileAcl.SetAccessRuleProtection($true,$false)
    $fileAcl.SetOwner((New-Object Security.Principal.SecurityIdentifier('S-1-5-32-544')))
    foreach ($id in @('S-1-5-18','S-1-5-32-544')) {
        $fileAcl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule((New-Object Security.Principal.SecurityIdentifier($id)),'FullControl','Allow')))
    }
    Set-Acl -LiteralPath $script -AclObject $fileAcl
    $escapedExe = [Security.SecurityElement]::Escape($exe)
    $escapedArguments = [Security.SecurityElement]::Escape($arguments)
    $xml = @"
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
 <RegistrationInfo><Description>Managed CU desktop: preserve the selected user's session after RDP disconnect. No automatic logon or unlock.</Description></RegistrationInfo>
 <Triggers><SessionStateChangeTrigger><Enabled>true</Enabled><UserId>$sid</UserId><Delay>PT3S</Delay><StateChange>RemoteDisconnect</StateChange></SessionStateChangeTrigger></Triggers>
 <Principals><Principal id="System"><UserId>S-1-5-18</UserId><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
 <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><StartWhenAvailable>true</StartWhenAvailable><Enabled>true</Enabled><ExecutionTimeLimit>PT1M</ExecutionTimeLimit></Settings>
 <Actions Context="System"><Exec><Command>$escapedExe</Command><Arguments>$escapedArguments</Arguments></Exec></Actions>
</Task>
"@
    Register-ScheduledTask -TaskName $name -Xml $xml -Force | Out-Null
    $task = Get-ScheduledTask -TaskName $name
}
if (-not $task -or -not $task.Settings.Enabled) { throw 'Desktop task missing or disabled' }
if ($task.Settings.MultipleInstances -ne 'IgnoreNew' -or $task.Settings.ExecutionTimeLimit -ne 'PT1M') { throw 'Desktop task execution policy drift' }
if (-not (Test-Path $script) -or (Get-FileHash -Algorithm SHA256 -LiteralPath $script).Hash.ToLowerInvariant() -ne $hash) { throw 'Desktop runtime hash drift' }
if (-not (Test-RestrictedAcl $root) -or -not (Test-RestrictedAcl $script)) { throw 'Desktop runtime ACL drift' }
$principal = $task.Principal.UserId
if ($principal -notin @('SYSTEM','S-1-5-18') -or $task.Principal.RunLevel -ne 'Highest') { throw 'Desktop task principal drift' }
if (@($task.Actions).Count -ne 1 -or $task.Actions[0].Execute -ne $exe -or $task.Actions[0].Arguments -ne $arguments) { throw 'Desktop task action drift' }
$triggers = @($task.Triggers)
if ($triggers.Count -ne 1 -or $triggers[0].StateChange -ne 4 -or -not $triggers[0].Enabled -or $triggers[0].Delay -ne 'PT3S') { throw 'Desktop task trigger drift' }
$triggerUser = $triggers[0].UserId
$triggerSid = if ($triggerUser -match '^S-1-') { $triggerUser } else { (New-Object Security.Principal.NTAccount($triggerUser)).Translate([Security.Principal.SecurityIdentifier]).Value }
if ($triggerSid -ne $sid) { throw 'Desktop task account drift' }
$snapshot = (& $exe -NoProfile -NonInteractive -File $script -TargetSid $sid -CheckOnly) | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'Desktop session query failed' }
$own = @($snapshot.sessions | Where-Object {$_.Sid -eq $sid})
$desktop = if ($own.Count -eq 0) { 'waiting-for-first-login' } elseif (@($own | Where-Object {$_.State -eq 0}).Count) { 'session-active-input-unverified' } else { 'session-disconnected' }
@{task=$name;configuration='ready';userSid=$sid;runtimeSha256=$hash;desktop=$desktop;sessions=$snapshot.sessions} | ConvertTo-Json -Depth 4 -Compress
