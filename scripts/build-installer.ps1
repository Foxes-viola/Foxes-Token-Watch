[CmdletBinding()]
param([switch]$KeepBuildFiles)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
Push-Location $projectRoot
try {
    if (-not (Test-Path -LiteralPath 'node_modules\@tauri-apps\cli')) {
        npm.cmd ci --no-audit --no-fund
        if ($LASTEXITCODE -ne 0) { throw 'Installing build dependencies failed.' }
    }

    npm.cmd run tauri -- build --bundles nsis -- --locked
    if ($LASTEXITCODE -ne 0) { throw 'Building the Windows installer failed.' }

    $config = Get-Content -LiteralPath 'src-tauri\tauri.conf.json' -Raw -Encoding UTF8 | ConvertFrom-Json
    $generatedInstaller = Get-ChildItem -LiteralPath 'target\release\bundle\nsis' -File -Filter '*-setup.exe' |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $generatedInstaller) { throw 'The NSIS installer was not generated.' }

    $architecture = if ($generatedInstaller.Name -match '_([^_]+)-setup\.exe$') { $Matches[1] } else { 'windows' }
    $outputDir = Join-Path $projectRoot 'installers'
    New-Item -ItemType Directory -Path $outputDir -Force | Out-Null
    $outputPath = Join-Path $outputDir "Foxes-Token-Watch-$($config.version)-$architecture-Setup.exe"
    Copy-Item -LiteralPath $generatedInstaller.FullName -Destination $outputPath -Force
    if ((Get-FileHash -LiteralPath $outputPath).Hash -ne (Get-FileHash -LiteralPath $generatedInstaller.FullName).Hash) {
        throw 'The installer copy failed verification.'
    }
    try {
        Copy-Item -LiteralPath 'target\release\Foxes-Token-Watch.exe' -Destination 'Foxes-watch.exe' -Force -ErrorAction Stop
    } catch {
        Write-Warning 'The installer is ready, but the portable executable could not be replaced. Close the app before updating it.'
    }

    if (-not $KeepBuildFiles) {
        & (Join-Path $PSScriptRoot 'clean-dev.ps1')
    }
    Write-Output "Installer: $outputPath"
} finally {
    Pop-Location
}
