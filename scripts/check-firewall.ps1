$ErrorActionPreference = 'Stop'
try {
    $names = 'DuoLink-v3-Tunnel', 'DuoLink-v3-App'
    $rules = @(Get-NetFirewallRule -PolicyStore ActiveStore | Where-Object { $_.Name -in $names })
    $tunnel = $rules | Where-Object { $_.Name -eq $names[0] -and $_.Enabled -eq 'True' -and $_.Direction -eq 'Inbound' -and $_.Action -eq 'Allow' }
    $app = $rules | Where-Object { $_.Name -eq $names[1] -and $_.Enabled -eq 'True' -and $_.Direction -eq 'Inbound' -and $_.Action -eq 'Allow' }
    if (-not $tunnel -or -not $app) { exit 1 }
    $port = $tunnel | Get-NetFirewallPortFilter
    $appPort = $app | Get-NetFirewallPortFilter
    $program = ($app | Get-NetFirewallApplicationFilter).Program
    if ($port.Protocol -ne 'UDP' -or $port.LocalPort -ne $env:DUOLINK_CHECK_PORT -or
        $appPort.Protocol -ne 'UDP' -or
        [Environment]::ExpandEnvironmentVariables($program) -ne $env:DUOLINK_CHECK_PROGRAM) { exit 1 }
    exit 0
}
catch {
    exit 2
}
