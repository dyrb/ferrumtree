param([switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$cargoRoot = if ($env:CARGO_HOME) { [IO.Path]::GetFullPath($env:CARGO_HOME) } else { Join-Path $env:USERPROFILE '.cargo' }
$oldFlags = $env:RUSTFLAGS
$oldEncoded = $env:CARGO_ENCODED_RUSTFLAGS
$oldJobs = $env:CARGO_BUILD_JOBS
Push-Location -LiteralPath $projectRoot
try {
    $version = (Select-String -LiteralPath (Join-Path $projectRoot 'Cargo.toml') -Pattern '^version = "([^"]+)"$').Matches[0].Groups[1].Value
    if (-not $SkipBuild) {
        $env:RUSTFLAGS = $null
        $env:CARGO_ENCODED_RUSTFLAGS = @("--remap-path-prefix=$projectRoot=FerrumTree", "--remap-path-prefix=$cargoRoot=cargo", '-C', 'link-arg=/DEBUG:NONE') -join [char]31
        if (-not $env:CARGO_BUILD_JOBS) { $env:CARGO_BUILD_JOBS = '2' }
        $ErrorActionPreference = 'Continue'
        cargo build --release --workspace --locked
        $buildResult = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
        if ($buildResult -ne 0) { throw "Build failed ($buildResult)" }
    }
    $gui = Join-Path $projectRoot 'target\release\ferrumtree-gui.exe'
    if ([Diagnostics.FileVersionInfo]::GetVersionInfo($gui).ProductVersion -ne $version) { throw 'GUI version does not match Cargo.toml' }
    $destination = Join-Path $projectRoot 'dist\FerrumTree'
    New-Item -ItemType Directory -Path $destination -Force | Out-Null
    $archiveFiles = @()
    # Explicit entries exclude personal settings, saved scans and old archives.
    foreach ($file in 'ferrumtree.exe', 'ferrumtree-gui.exe') {
        $source = Join-Path $projectRoot "target\release\$file"
        Copy-Item -LiteralPath $source -Destination $destination -Force
        $archiveFiles += [pscustomobject]@{ Source = Join-Path $destination $file; Name = $file }
    }
    foreach ($file in 'README.md', 'README.ru.md', 'CHANGELOG.md', 'LICENSE-MIT', 'LICENSE-APACHE', 'THIRD_PARTY_NOTICES.md') {
        Copy-Item -LiteralPath (Join-Path $projectRoot $file) -Destination $destination -Force
        $archiveFiles += [pscustomobject]@{ Source = Join-Path $destination $file; Name = $file }
    }
    foreach ($file in 'screenshot-en.png', 'screenshot-ru.png') {
        $source = Join-Path $projectRoot "docs\$file"
        if (Test-Path -LiteralPath $source -PathType Leaf) {
            $archiveFiles += [pscustomobject]@{ Source = $source; Name = "docs/$file" }
        }
    }
    $archiveName = "FerrumTree-$version-x64.zip"
    $archive = Join-Path $projectRoot "dist\$archiveName"
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $stream = [IO.File]::Open($archive, [IO.FileMode]::Create, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try {
        $zip = [IO.Compression.ZipArchive]::new($stream, [IO.Compression.ZipArchiveMode]::Create, $true)
        try {
            foreach ($file in $archiveFiles) {
                [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, $file.Source, $file.Name, [IO.Compression.CompressionLevel]::Optimal)
            }
        } finally { $zip.Dispose() }
    } finally { $stream.Dispose() }
    $hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText("$archive.sha256", "$hash  $archiveName`n", [Text.Encoding]::ASCII)
    Write-Output $archive
    Write-Output "SHA-256 $hash"
} finally {
    $env:RUSTFLAGS = $oldFlags
    $env:CARGO_ENCODED_RUSTFLAGS = $oldEncoded
    $env:CARGO_BUILD_JOBS = $oldJobs
    Pop-Location
}
