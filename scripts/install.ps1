# Elevated part of DuoLink setup. Started by DuoLink.exe through a single UAC prompt.
# Every installer is verified against the SHA-256 pinned in DuoLink before it runs.
param(
    [string]$ViGEm = '',
    [string]$ViGEmSha = '',
    [string]$Tailscale = '',
    [string]$TailscaleSha = '',
    [switch]$Firewall,
    [string]$Log = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($Log) { Start-Transcript -LiteralPath $Log -Force | Out-Null }

function Assert-Hash([string]$Path, [string]$Expected) {
    $actual = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
    if ($actual -ne $Expected.ToUpperInvariant()) { throw "Checksum mismatch: $Path" }
}

function Assert-Success([string]$Name, $Process) {
    # 3010 and 1641 mean success with a pending reboot.
    if ($Process.ExitCode -notin 0, 3010, 1641) { throw "$Name installer exited with code $($Process.ExitCode)" }
}

try {
    if ($ViGEm) {
        Assert-Hash $ViGEm $ViGEmSha
        Write-Output 'Installing ViGEmBus'
        Assert-Success 'ViGEmBus' (Start-Process -FilePath $ViGEm -ArgumentList '/exenoui', '/qn', '/norestart' -Wait -PassThru)
    }
    if ($Tailscale) {
        Assert-Hash $Tailscale $TailscaleSha
        Write-Output 'Installing Tailscale'
        Assert-Success 'Tailscale' (Start-Process -FilePath "$env:SystemRoot\System32\msiexec.exe" -ArgumentList '/i', "`"$Tailscale`"", '/qn', '/norestart' -Wait -PassThru)
    }
    if ($Firewall) {
        Write-Output 'Configuring firewall'
        Get-NetFirewallRule -Name 'DuoLink-v1-*', 'DuoLink-v2-*' -ErrorAction SilentlyContinue | Remove-NetFirewallRule
        # Port rules (not program rules) keep working when DuoLink or an engine is updated.
        # Only the tailnet and the local subnet can reach them.
        $scope = @{
            Direction     = 'Inbound'
            Action        = 'Allow'
            Profile       = 'Any'
            RemoteAddress = @('100.64.0.0/10', 'LocalSubnet')
            Enabled       = 'True'
        }
        New-NetFirewallRule -Name 'DuoLink-v2-TCP' -DisplayName 'DuoLink (TCP)' -Protocol TCP -LocalPort 48171, 48184, 48189, 48210 @scope | Out-Null
        New-NetFirewallRule -Name 'DuoLink-v2-UDP' -DisplayName 'DuoLink (UDP)' -Protocol UDP -LocalPort 48198, 48199, 48200 @scope | Out-Null
    }
    exit 0
}
catch {
    Write-Output $_.Exception.Message
    exit 1
}
finally {
    if ($Log) { Stop-Transcript | Out-Null }
}
