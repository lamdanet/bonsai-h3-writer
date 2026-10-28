<#
.SYNOPSIS
    Builds Bonsai H3 Writer and packages it the same way as a GitHub release.

.DESCRIPTION
    Local equivalent of .github/workflows/release.yml (the workflow runs this script with
    -SkipBuild after its own build step):

      1. cargo build --release --locked
      2. dist\BonsaiH3Writer-<version>-windows-x64.zip with bonsai-h3-writer.exe, bh3.exe,
         README.md, LICENSE, CHANGELOG.md, docs\CLI.md, docs\AGENT_GUIDE.md and docs\MCP.md
         (the script stops with an error when one of them is missing)
      3. dist\BonsaiH3Writer-<version>-windows-x64.zip.sha256 in sha256sum format
         ("<lowercase hex>  <zip file name>")

    <version> is the [package] version in Cargo.toml. Existing files with the same names are
    replaced; other files in the output folder are left alone.
    Works in Windows PowerShell 5.1 and PowerShell 7.

.PARAMETER SkipBuild
    Package the executables already in cargo's release folder instead of building them first:
    target\release, unless CARGO_TARGET_DIR, CARGO_BUILD_TARGET_DIR or a .cargo\config.toml
    `[build] target-dir` moves it (the script asks `cargo metadata`, so cargo is needed either way).

.PARAMETER OutDir
    Output folder. Default: dist in the repository root.

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\package.ps1

.EXAMPLE
    .\scripts\package.ps1 -SkipBuild -OutDir C:\Temp\bh3-dist
#>
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [string]$OutDir = ''
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot

function Get-CrateVersion {
    # Reads `version = "..."` from the [package] section of Cargo.toml.
    param([Parameter(Mandatory = $true)][string]$CargoToml)
    $section = ''
    foreach ($line in Get-Content -LiteralPath $CargoToml) {
        if ($line -match '^\s*\[') {
            $section = $line.Trim()
            continue
        }
        if ($section -eq '[package]' -and $line -match '^\s*version\s*=\s*"([^"]+)"') {
            return $Matches[1]
        }
    }
    throw "No version found in the [package] section of $CargoToml"
}

function Invoke-Cargo {
    # Runs cargo in the repository root and throws when it fails. With -Capture it returns
    # cargo's stdout instead of showing it.
    param(
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [switch]$Capture
    )
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw 'cargo was not found. Install Rust from https://rustup.rs and open a new terminal.'
    }
    $commandLine = 'cargo ' + ($Arguments -join ' ')
    Write-Host "==> $commandLine"
    # cargo reports progress on stderr; with 'Stop', Windows PowerShell can turn that into an error.
    $savedPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $savedEncoding = $null
    if ($Capture) {
        # cargo writes UTF-8, which Windows PowerShell 5.1 would read in the OEM code page (a
        # non-ASCII path would come back garbled). Without a console the default stays.
        try {
            $savedEncoding = [Console]::OutputEncoding
            [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding -ArgumentList $false
        }
        catch {
            $savedEncoding = $null
        }
    }
    $output = $null
    Push-Location -LiteralPath $root
    try {
        if ($Capture) {
            $output = (& cargo @Arguments) -join "`n"
        }
        else {
            & cargo @Arguments
        }
        $exitCode = $LASTEXITCODE
    }
    finally {
        Pop-Location
        if ($null -ne $savedEncoding) {
            [Console]::OutputEncoding = $savedEncoding
        }
        $ErrorActionPreference = $savedPreference
    }
    if ($exitCode -ne 0) {
        throw "$commandLine failed with exit code $exitCode"
    }
    return $output
}

function Get-CargoTargetDir {
    # The folder cargo builds into. CARGO_TARGET_DIR, CARGO_BUILD_TARGET_DIR and `[build] target-dir`
    # in a .cargo\config.toml can all move it; `cargo metadata` reports the result.
    $metadata = Invoke-Cargo -Arguments @('metadata', '--format-version', '1', '--no-deps') -Capture
    $dir = ($metadata | ConvertFrom-Json).target_directory
    if (-not $dir) {
        throw 'cargo metadata did not report a target directory.'
    }
    return $dir
}

function Add-FileToZip {
    # Adds one file under an explicit entry name ('/' separators on every PowerShell version).
    param(
        [Parameter(Mandatory = $true)]$Archive,
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$EntryName
    )
    $entry = $Archive.CreateEntry($EntryName, [System.IO.Compression.CompressionLevel]::Optimal)
    $entry.LastWriteTime = (Get-Item -LiteralPath $Source).LastWriteTime
    $target = $entry.Open()
    try {
        $sourceStream = [System.IO.File]::OpenRead($Source)
        try {
            $sourceStream.CopyTo($target)
        }
        finally {
            $sourceStream.Dispose()
        }
    }
    finally {
        $target.Dispose()
    }
}

$version = Get-CrateVersion -CargoToml (Join-Path $root 'Cargo.toml')
$packageName = "BonsaiH3Writer-$version-windows-x64"

if ($OutDir) {
    $outPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutDir)
}
else {
    $outPath = Join-Path $root 'dist'
}

# The folder the build below (or the workflow's own build step) writes to.
$releaseDir = Join-Path (Get-CargoTargetDir) 'release'

if ($SkipBuild) {
    Write-Host "==> Using the existing build in $releaseDir"
}
else {
    Invoke-Cargo -Arguments @('build', '--release', '--locked')
}

# Zip entry name -> source file. Every release has all of them: README.md and docs\CLI.md link to
# the other docs, so a missing file stops the script instead of shipping a broken link.
$contents = @(
    [pscustomobject]@{ Entry = 'bonsai-h3-writer.exe'; Source = (Join-Path $releaseDir 'bonsai-h3-writer.exe') }
    [pscustomobject]@{ Entry = 'bh3.exe'; Source = (Join-Path $releaseDir 'bh3.exe') }
    [pscustomobject]@{ Entry = 'README.md'; Source = (Join-Path $root 'README.md') }
    [pscustomobject]@{ Entry = 'LICENSE'; Source = (Join-Path $root 'LICENSE') }
    [pscustomobject]@{ Entry = 'CHANGELOG.md'; Source = (Join-Path $root 'CHANGELOG.md') }
    [pscustomobject]@{ Entry = 'docs/CLI.md'; Source = (Join-Path $root 'docs\CLI.md') }
    [pscustomobject]@{ Entry = 'docs/AGENT_GUIDE.md'; Source = (Join-Path $root 'docs\AGENT_GUIDE.md') }
    [pscustomobject]@{ Entry = 'docs/MCP.md'; Source = (Join-Path $root 'docs\MCP.md') }
)

$missing = @($contents | Where-Object { -not (Test-Path -LiteralPath $_.Source -PathType Leaf) })
if ($missing.Count -gt 0) {
    $message = "Not packaged - $($missing.Count) file(s) missing:"
    foreach ($item in $missing) {
        $message += "`n  $($item.Source)"
    }
    if (@($missing | Where-Object { $_.Entry -like '*.exe' }).Count -gt 0) {
        $message += "`nBuild the executables with ""cargo build --release --locked"" or run this script without -SkipBuild."
    }
    throw $message
}

New-Item -ItemType Directory -Force -Path $outPath | Out-Null
$zipPath = Join-Path $outPath "$packageName.zip"
$shaPath = "$zipPath.sha256"
foreach ($old in @($zipPath, $shaPath)) {
    if (Test-Path -LiteralPath $old) {
        Remove-Item -LiteralPath $old -Force
    }
}

Write-Host "==> Writing $zipPath"
Add-Type -AssemblyName System.IO.Compression
$zipStream = [System.IO.File]::Open($zipPath, [System.IO.FileMode]::CreateNew)
try {
    $archive = New-Object System.IO.Compression.ZipArchive -ArgumentList $zipStream, ([System.IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($item in $contents) {
            Add-FileToZip -Archive $archive -Source $item.Source -EntryName $item.Entry
        }
    }
    finally {
        $archive.Dispose()
    }
}
finally {
    $zipStream.Dispose()
}

$hash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
$utf8NoBom = New-Object System.Text.UTF8Encoding -ArgumentList $false
[System.IO.File]::WriteAllText($shaPath, "$hash  $packageName.zip`n", $utf8NoBom)

$sizeMb = (Get-Item -LiteralPath $zipPath).Length / 1MB
Write-Host ''
Write-Host "Bonsai H3 Writer $version packaged:"
foreach ($item in $contents) {
    Write-Host "  $($item.Entry)"
}
Write-Host ''
Write-Host ('Zip     {0} ({1:N1} MB)' -f $zipPath, $sizeMb)
Write-Host ('SHA256  {0}' -f $shaPath)
Write-Host ('        {0}' -f $hash)
