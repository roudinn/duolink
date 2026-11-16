param(
    [Parameter(Mandatory)]
    [string[]]$Assets,
    [Parameter(Mandatory)]
    [string]$Output
)
$ErrorActionPreference = 'Stop'
$files = $Assets | ForEach-Object { Get-Item -LiteralPath $_ } | Sort-Object Name
if (($files | Group-Object Name | Where-Object Count -gt 1)) {
    throw 'Release asset names must be unique'
}
$lines = foreach ($file in $files) {
    $hash = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $($file.Name)"
}
[IO.File]::WriteAllText($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Output), ($lines -join "`n") + "`n")
