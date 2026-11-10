# Per-user registration of an installed DuoLink.exe: shortcuts and the Apps & Features entry.
param(
    [Parameter(Mandatory)][string]$Exe,
    [string]$Version = '',
    [switch]$Remove
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$links = @(
    (Join-Path ([Environment]::GetFolderPath('Desktop')) 'DuoLink.lnk'),
    (Join-Path ([Environment]::GetFolderPath('Programs')) 'DuoLink.lnk')
)
$key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\DuoLink'

if ($Remove) {
    foreach ($link in $links) { Remove-Item -LiteralPath $link -Force -ErrorAction SilentlyContinue }
    Remove-Item -LiteralPath $key -Recurse -Force -ErrorAction SilentlyContinue
    exit 0
}

$folder = Split-Path -LiteralPath $Exe
$shell = New-Object -ComObject WScript.Shell
foreach ($link in $links) {
    $shortcut = $shell.CreateShortcut($link)
    $shortcut.TargetPath = $Exe
    $shortcut.WorkingDirectory = $folder
    $shortcut.IconLocation = "$Exe,0"
    $shortcut.Description = 'Игра вдвоём по сети'
    $shortcut.Save()
}

New-Item -Path $key -Force | Out-Null
$values = @{
    DisplayName     = 'DuoLink'
    DisplayVersion  = $Version
    Publisher       = 'DuoLink'
    DisplayIcon     = "$Exe,0"
    InstallLocation = $folder
    UninstallString = "`"$Exe`" --uninstall"
}
foreach ($name in $values.Keys) { Set-ItemProperty -Path $key -Name $name -Value $values[$name] }
foreach ($name in 'NoModify', 'NoRepair') { New-ItemProperty -Path $key -Name $name -Value 1 -PropertyType DWord -Force | Out-Null }
New-ItemProperty -Path $key -Name 'EstimatedSize' -Value ([int]((Get-Item -LiteralPath $Exe).Length / 1KB)) -PropertyType DWord -Force | Out-Null
exit 0
