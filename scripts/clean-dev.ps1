[CmdletBinding(SupportsShouldProcess)]
param()

$ErrorActionPreference = 'Stop'
$projectRoot = [System.IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot)).TrimEnd('\')
$portableExe = Join-Path $projectRoot 'Foxes-watch.exe'
if (-not (Test-Path -LiteralPath $portableExe -PathType Leaf)) {
    throw 'Build and copy Foxes-watch.exe to the project root before cleaning.'
}

# These paths contain generated builds, downloaded dependencies and old copies.
# Keep the current executable, source, Git history and application data.
$generatedPaths = @(
    'target', 'src-tauri\target', 'src-tauri\gen', 'dist',
    'node_modules', 'tmp', 'debug', 'release', 'Foxes-watch.old.exe'
)
$cleanupTargets = foreach ($relativePath in $generatedPaths) {
    $candidate = [System.IO.Path]::GetFullPath((Join-Path $projectRoot $relativePath))
    if (-not $candidate.StartsWith($projectRoot + '\', [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Cleanup path escaped the project: $candidate"
    }
    if (-not (Test-Path -LiteralPath $candidate)) { continue }
    $entry = Get-Item -LiteralPath $candidate -Force
    if ($entry.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
        throw "Refusing to clean a linked path: $candidate"
    }
    # Git refuses a repository on a file system that records no owner (exFAT);
    # trust this one for this command only.
    $trackedFiles = @(git -c "safe.directory=$($projectRoot -replace '\\', '/')" -C $projectRoot ls-files -- $relativePath)
    if ($LASTEXITCODE -ne 0 -or $trackedFiles.Count -gt 0) {
        throw "Cannot verify that this path contains no tracked source: $candidate"
    }
    $entry
}

$removedBytes = 0L
foreach ($entry in $cleanupTargets) {
    $bytes = if ($entry.PSIsContainer) {
        (Get-ChildItem -LiteralPath $entry.FullName -Recurse -File -Force | Measure-Object Length -Sum).Sum
    } else {
        $entry.Length
    }
    if ($PSCmdlet.ShouldProcess($entry.FullName, 'Remove generated development files')) {
        Remove-Item -LiteralPath $entry.FullName -Recurse -Force
        $removedBytes += $bytes
    }
}

Write-Output ('Removed {0:N2} MiB of development files.' -f ($removedBytes / 1MB))
