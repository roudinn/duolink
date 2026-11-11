# Per-user registration of an installed DuoLink.exe: shortcuts and the Apps & Features entry.
param(
    [Parameter(Mandatory)][string]$Exe,
    [string]$Version = '',
    [switch]$Remove,
    [int]$ParentId = 0
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$links = @(
    (Join-Path ([Environment]::GetFolderPath('Desktop')) 'DuoLink.lnk'),
    (Join-Path ([Environment]::GetFolderPath('Programs')) 'DuoLink.lnk')
)
$key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\DuoLink'

function Remove-DuoLinkFiles([string]$Local, [string]$InstalledExe) {
    if (!$Local) { throw 'Не найдена папка LocalAppData.' }
    $programs = Join-Path $Local 'Programs'
    $installed = Join-Path $programs 'DuoLink'
    $data = Join-Path $Local 'DuoLink'
    $expected = Join-Path $installed 'DuoLink.exe'
    if (![string]::Equals([IO.Path]::GetFullPath($InstalledExe), $expected, 'OrdinalIgnoreCase')) {
        throw 'Запустите удаление DuoLink из списка приложений Windows.'
    }

    foreach ($path in @($programs, $installed, $data)) {
        if (!(Test-Path -LiteralPath $path)) { continue }
        $item = Get-Item -LiteralPath $path -Force
        if (!$item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw "Папка $path перенаправлена."
        }
        $resolved = (Resolve-Path -LiteralPath $path).ProviderPath
        if (![string]::Equals($resolved, $path, 'OrdinalIgnoreCase')) {
            throw "Не удалось проверить путь $path."
        }
    }

    foreach ($path in @($data, $installed)) {
        if ([IO.Directory]::Exists($path)) {
            # On Windows PowerShell 5.1 recursive deletion can fail on a junction.
            # Remove directory links themselves before deleting the contents.
            $pending = [Collections.Generic.Stack[string]]::new()
            $pending.Push($path)
            while ($pending.Count -gt 0) {
                foreach ($child in [IO.Directory]::EnumerateDirectories($pending.Pop())) {
                    if ([IO.File]::GetAttributes($child) -band [IO.FileAttributes]::ReparsePoint) {
                        [IO.Directory]::Delete($child, $false)
                    } else {
                        $pending.Push($child)
                    }
                }
            }
            [IO.Directory]::Delete($path, $true)
        }
    }
}

if ($Remove) {
    Add-Type -AssemblyName System.Windows.Forms
    try {
        $parent = Get-Process -Id $ParentId -ErrorAction SilentlyContinue
        if ($parent) { Wait-Process -InputObject $parent -Timeout 15 }
        $running = Get-Process -Name 'DuoLink' -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $Exe }
        if ($running) { throw 'Закройте DuoLink и повторите.' }

        Remove-DuoLinkFiles ([Environment]::GetFolderPath('LocalApplicationData')) $Exe
        foreach ($link in $links) {
            if (Test-Path -LiteralPath $link) { Remove-Item -LiteralPath $link -Force }
        }
        if (Test-Path -LiteralPath $key) { Remove-Item -LiteralPath $key -Recurse -Force }
        [void][System.Windows.Forms.MessageBox]::Show('DuoLink удалён.', 'DuoLink', 'OK', 'Information')
        exit 0
    }
    catch {
        $message = "Не удалось удалить DuoLink.`r`n$($_.Exception.Message)"
        [void][System.Windows.Forms.MessageBox]::Show($message, 'DuoLink', 'OK', 'Error')
        exit 1
    }
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
