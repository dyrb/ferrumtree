# Build the release binaries and pack them into dist\FerrumTree-<version>-x64.zip with a
# SHA-256 checksum. Paths of the build machine are mapped to neutral names, so the
# binaries do not contain them.

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$env:RUSTFLAGS = "--remap-path-prefix=$cargoHome=cargo --remap-path-prefix=$root=ferrumtree -C link-arg=/DEBUG:NONE"

# Cargo reports progress on stderr, which Windows PowerShell would treat as an error.
$ErrorActionPreference = 'Continue'
cargo build --release --locked --manifest-path (Join-Path $root 'Cargo.toml')
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$ErrorActionPreference = 'Stop'

$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$dist = Join-Path $root 'dist'
$name = "FerrumTree-$version-x64"
$stage = Join-Path $dist $name
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force $stage | Out-Null

$bin = Join-Path $root 'target\release'
Copy-Item (Join-Path $bin 'ferrumtree-gui.exe'), (Join-Path $bin 'ferrumtree.exe') $stage
Copy-Item (Join-Path $root 'README.md'), (Join-Path $root 'LICENSE-MIT'), (Join-Path $root 'LICENSE-APACHE'), (Join-Path $root 'CHANGELOG.md') $stage

$zip = Join-Path $dist "$name.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip
$hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
Set-Content -Path "$zip.sha256" -Value "$hash  $name.zip" -Encoding ascii -NoNewline
Remove-Item $stage -Recurse -Force
Write-Output "$zip"
Write-Output "SHA-256 $hash"
