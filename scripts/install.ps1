# Elevated part of DuoLink setup. Started by DuoLink.exe through a single UAC prompt.
# Every installer is verified against the SHA-256 pinned in DuoLink before it runs.
param(
    [string]$ViGEm = '',
    [string]$ViGEmSha = '',
    [string]$VirtualHid = '',
    [string]$VirtualHidSha = '',
    [switch]$Firewall,
    [int]$Port = 48170,
    [string]$Program = '',
    [string]$Log = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($Log) { Start-Transcript -LiteralPath $Log -Force | Out-Null }

function Assert-Hash([string]$Path, [string]$Expected) {
    $actual = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
    if ($actual -ne $Expected.ToUpperInvariant()) { throw "Checksum mismatch: $Path" }
}

try {
    $restartNeeded = $false
    if ($ViGEm) {
        Assert-Hash $ViGEm $ViGEmSha
        Write-Output 'Installing ViGEmBus'
        $process = Start-Process -FilePath $ViGEm -ArgumentList '/exenoui', '/qn', '/norestart' -WindowStyle Hidden -Wait -PassThru
        # 3010 and 1641 mean success with a pending reboot.
        if ($process.ExitCode -notin 0, 3010, 1641) { throw "ViGEmBus installer exited with code $($process.ExitCode)" }
        $restartNeeded = $process.ExitCode -in 3010, 1641
    }
    if ($VirtualHid) {
        Assert-Hash $VirtualHid $VirtualHidSha
        Write-Output 'Installing Virtual HID Broker'
        $arguments = '/i "{0}" /qn /norestart' -f $VirtualHid
        $process = Start-Process -FilePath "$env:SystemRoot\System32\msiexec.exe" -ArgumentList $arguments -WindowStyle Hidden -Wait -PassThru
        if ($process.ExitCode -notin 0, 3010, 1641) { throw "Virtual HID Broker installer exited with code $($process.ExitCode)" }
        $restartNeeded = $restartNeeded -or $process.ExitCode -in 3010, 1641
    }
    if ($Firewall) {
        Write-Output 'Configuring firewall'
        Get-NetFirewallRule -Name 'DuoLink-v1-*', 'DuoLink-v2-*', 'DuoLink-v3-*' -ErrorAction SilentlyContinue | Remove-NetFirewallRule
        # A rule left over from a dismissed Windows prompt would block DuoLink outright.
        if ($Program) {
            Get-NetFirewallApplicationFilter -Program $Program -ErrorAction SilentlyContinue |
                Get-NetFirewallRule | Where-Object { $_.Action -eq 'Block' -and $_.Direction -eq 'Inbound' } |
                Remove-NetFirewallRule
        }
        # One UDP port carries the whole stream. Sunshine itself only listens on 127.0.0.1.
        New-NetFirewallRule -Name 'DuoLink-v3-Tunnel' -DisplayName 'DuoLink' -Direction Inbound -Action Allow `
            -Protocol UDP -LocalPort $Port -Profile Any | Out-Null
        if ($Program) {
            New-NetFirewallRule -Name 'DuoLink-v3-App' -DisplayName 'DuoLink (приложение)' -Direction Inbound -Action Allow `
                -Program $Program -Protocol UDP -Profile Any | Out-Null
        }
    }
    if ($restartNeeded) { exit 20 }
    exit 0
}
catch {
    Write-Output $_.Exception.Message
    exit 1
}
finally {
    if ($Log) { Stop-Transcript | Out-Null }
}
