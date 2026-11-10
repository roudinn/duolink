[CmdletBinding()]
param(
    [switch]$Elevated,
    [switch]$Remove,
    [System.Net.IPAddress]$LanIp,
    [string]$MediaProgram
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$prefix = 'DuoLink-v1'
$ruleNames = "$prefix-Invite", "$prefix-Media-TCP", "$prefix-Media-UDP"

try {
    if (!$Elevated) {
        $data = $env:DUOLINK_DATA_DIR
        if (!$data) { $data = Join-Path $env:LOCALAPPDATA 'DuoLink' }
        $data = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($data)
        $MediaProgram = Join-Path $data 'engines\sunshine-2026.914.233613\Sunshine\sunshine.exe'
    }
    $user = [Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())
    if (!$user.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        if ($Elevated) { throw 'The firewall configuration requires administrator rights.' }
        $quotedScript = $PSCommandPath.Replace("'", "''")
        $invocation = "& '$quotedScript' -Elevated"
        if ($Remove) { $invocation += ' -Remove' }
        if ($LanIp) { $invocation += " -LanIp '$LanIp'" }
        $quotedProgram = $MediaProgram.Replace("'", "''")
        $invocation += " -MediaProgram '$quotedProgram'"
        $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($invocation))
        Write-Host 'Approve the Windows elevation prompt to configure the DuoLink host.'
        $task = Start-Process -FilePath "$PSHOME\powershell.exe" -Verb RunAs -Wait -PassThru -ArgumentList @(
            '-NoLogo', '-NoProfile', '-ExecutionPolicy', 'Bypass', '-EncodedCommand', $encoded
        )
        exit $task.ExitCode
    }

    if ($Remove) {
        Get-NetFirewallRule -Name $ruleNames -ErrorAction SilentlyContinue | Remove-NetFirewallRule
        Write-Host 'DuoLink firewall rules removed.'
    }
    else {
        $addresses = Get-NetIPAddress -AddressFamily IPv4 -AddressState Preferred
        if ($LanIp) {
            if ($LanIp.ToString() -notmatch '^(10\.|192\.168\.|172\.(1[6-9]|2[0-9]|3[01])\.)') {
                throw 'Specify a private LAN IPv4 address.'
            }
            $address = $addresses | Where-Object IPAddress -eq $LanIp.ToString() | Select-Object -First 1
            $remote = 'LocalSubnet'
            $profile = 'Private'
            if (!$address) { throw 'This LAN address is not active on the host.' }
        }
        else {
            $address = $addresses | Where-Object {
                $_.InterfaceAlias -match 'Tailscale' -and $_.IPAddress -match '^100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.'
            } | Select-Object -First 1
            $remote = '100.64.0.0/10'
            $profile = 'Any'
            if (!$address) { throw 'Connect Tailscale on this PC, then run Configure-Host again.' }
        }

        $duolink = Join-Path $PSScriptRoot 'DuoLink.exe'
        if (!(Test-Path -LiteralPath $duolink)) { throw 'Extract Configure-Host next to DuoLink.exe first.' }
        if (!$MediaProgram) { throw 'Launch Configure-Host.cmd from the original DuoLink folder.' }
        $scope = @{
            Enabled = 'True'
            Action = 'Allow'
            Direction = 'Inbound'
            LocalAddress = $address.IPAddress
            RemoteAddress = $remote
            InterfaceAlias = $address.InterfaceAlias
            Profile = $profile
        }
        $rules = @(
            @{ Name = $ruleNames[0]; Protocol = 'TCP'; LocalPort = '48171'; Program = $duolink },
            @{ Name = $ruleNames[1]; Protocol = 'TCP'; LocalPort = '48184','48189','48210'; Program = $MediaProgram },
            @{ Name = $ruleNames[2]; Protocol = 'UDP'; LocalPort = '48198','48199','48200'; Program = $MediaProgram }
        )
        foreach ($rule in $rules) {
            if (Get-NetFirewallRule -Name $rule.Name -ErrorAction SilentlyContinue) {
                Set-NetFirewallRule @rule @scope | Out-Null
            }
            else {
                New-NetFirewallRule @rule @scope -DisplayName $rule.Name | Out-Null
            }
        }
        Write-Host "DuoLink host allowed on $($address.InterfaceAlias) ($($address.IPAddress))."
    }
    if ($Elevated) { Read-Host 'Press Enter to finish' | Out-Null }
    exit 0
}
catch {
    Write-Host $_.Exception.Message -ForegroundColor Red
    if ($Elevated) { Read-Host 'Press Enter to finish' | Out-Null }
    exit 1
}
