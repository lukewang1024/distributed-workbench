param([Parameter(Mandatory=$true)][string]$TargetSid,[switch]$CheckOnly)
$ErrorActionPreference='Stop'
Add-Type @'
using System; using System.Runtime.InteropServices;
public class CuSessions {
 [StructLayout(LayoutKind.Sequential)] public struct Row { public int Id; public IntPtr Name; public int State; }
 [DllImport("wtsapi32.dll",CharSet=CharSet.Unicode,SetLastError=true)] public static extern bool WTSEnumerateSessions(IntPtr server,int reserved,int version,out IntPtr data,out int count);
 [DllImport("wtsapi32.dll")] public static extern void WTSFreeMemory(IntPtr p);
 [DllImport("wtsapi32.dll",CharSet=CharSet.Unicode,SetLastError=true)] public static extern bool WTSQuerySessionInformation(IntPtr server,int session,int cls,out IntPtr data,out int bytes);
 [DllImport("kernel32.dll")] public static extern uint WTSGetActiveConsoleSessionId();
 public static string Query(int session,int cls) { IntPtr p;int n;if(!WTSQuerySessionInformation(IntPtr.Zero,session,cls,out p,out n))throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());try{return Marshal.PtrToStringUni(p);}finally{WTSFreeMemory(p);} }
}
'@
function Get-CuSessions {
 $p=[IntPtr]::Zero;$count=0
 if(-not [CuSessions]::WTSEnumerateSessions([IntPtr]::Zero,0,1,[ref]$p,[ref]$count)){throw 'Cannot enumerate Windows sessions'}
 $size=[Runtime.InteropServices.Marshal]::SizeOf([type][CuSessions+Row])
 try{for($i=0;$i -lt $count;$i++){
  $row=[Runtime.InteropServices.Marshal]::PtrToStructure([IntPtr]($p.ToInt64()+$i*$size),[type][CuSessions+Row])
  if($row.Id -eq 0 -or $row.State -gt 4){continue}
  $user=[CuSessions]::Query($row.Id,5);if(-not $user){continue}
  $domain=[CuSessions]::Query($row.Id,7)
  $sid=(New-Object Security.Principal.NTAccount($domain,$user)).Translate([Security.Principal.SecurityIdentifier]).Value
  [pscustomobject]@{Id=$row.Id;State=$row.State;Sid=$sid;Station=[Runtime.InteropServices.Marshal]::PtrToStringUni($row.Name)}
 }}finally{[CuSessions]::WTSFreeMemory($p)}
}
function Write-CuLog($message) {
 $path=Join-Path $PSScriptRoot 'console-handoff.log'
 if((Test-Path $path) -and (Get-Item $path).Length -gt 1048576){Move-Item $path ($path+'.previous') -Force}
 Add-Content -LiteralPath $path -Value ((Get-Date -Format o)+' '+$message) -Encoding UTF8
}
try {
 $rows=@(Get-CuSessions)
 $candidates=@($rows | Where-Object {$_.Sid -eq $TargetSid -and $_.State -eq 4})
 $active=@($rows | Where-Object {$_.State -eq 0})
 if($CheckOnly){@{sessions=$rows;candidateCount=$candidates.Count;activeCount=$active.Count;console=[CuSessions]::WTSGetActiveConsoleSessionId()} | ConvertTo-Json -Depth 4 -Compress;exit 0}
 if($active.Count -gt 0){Write-CuLog 'skip: an interactive user session is active';exit 0}
 if($candidates.Count -ne 1){Write-CuLog ('skip: disconnected target session count='+$candidates.Count);exit 0}
 $target=$candidates[0].Id
 # Recheck immediately before transfer, in case the user reconnected.
 $fresh=@(Get-CuSessions)
 if(@($fresh | Where-Object {$_.State -eq 0}).Count -gt 0 -or @($fresh | Where-Object {$_.Id -eq $target -and $_.Sid -eq $TargetSid -and $_.State -eq 4}).Count -ne 1){Write-CuLog 'skip: session changed';exit 0}
 & (Join-Path $env:SystemRoot 'System32\tscon.exe') "$target" '/dest:console'
 if($LASTEXITCODE -ne 0){throw "tscon exit $LASTEXITCODE"}
 Start-Sleep -Seconds 2
 if([CuSessions]::WTSGetActiveConsoleSessionId() -ne $target){throw 'Console transfer could not be verified'}
 Write-CuLog "success: session $target transferred to console"
} catch {Write-CuLog ('error: '+$_.Exception.Message);throw}
