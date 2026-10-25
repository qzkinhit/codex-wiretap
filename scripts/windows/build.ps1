[CmdletBinding()]
param(
    [string]$ReleaseTag = '',
    [string]$OutputDir = '',
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($env:OS -ne 'Windows_NT') { throw 'Build this installer on Windows x64.' }
if (-not [Environment]::Is64BitOperatingSystem) { throw 'A 64-bit Windows host is required.' }

$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$target = 'x86_64-pc-windows-msvc'
# Bundle the MSVC runtime into our binaries, so users do not need a separate
# Visual C++ Redistributable installation. CI uses the same flags for testing.
$flagName = 'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS'
$originalFlags = [Environment]::GetEnvironmentVariable($flagName, 'Process')
[Environment]::SetEnvironmentVariable($flagName, ($originalFlags + ' -C target-feature=+crt-static').Trim(), 'Process')
if ([string]::IsNullOrWhiteSpace($OutputDir)) { $OutputDir = Join-Path $repoRoot 'dist\windows' }
$OutputDir = [IO.Path]::GetFullPath($OutputDir)

function Assert-X64Executable([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "Missing executable: $Path" }
    $stream = [IO.File]::OpenRead($Path)
    $reader = [IO.BinaryReader]::new($stream)
    try {
        if ($reader.ReadUInt16() -ne 0x5A4D) { throw "Not a PE executable: $Path" }
        $stream.Position = 0x3C
        $peOffset = $reader.ReadInt32()
        $stream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x00004550 -or $reader.ReadUInt16() -ne 0x8664) {
            throw "Expected a Windows x64 PE executable: $Path"
        }
    }
    finally { $reader.Dispose(); $stream.Dispose() }
}

function Write-DependencyLicenses([string]$PayloadPath) {
    $allMetadataText = & cargo metadata --format-version 1 --filter-platform $target --locked
    if ($LASTEXITCODE -ne 0) { throw 'Could not resolve dependency license metadata.' }
    $allMetadata = $allMetadataText | ConvertFrom-Json
    $resolvedIds = @($allMetadata.resolve.nodes | ForEach-Object { $_.id })
    $notices = [Collections.Generic.List[string]]::new()
    $notices.Add('Pool Router third-party dependencies')
    $notices.Add('Public package metadata and bundled license files from the locked Windows dependency graph (including build/test dependencies).')
    $licenseRoot = Join-Path $PayloadPath 'licenses'
    New-Item -ItemType Directory -Path $licenseRoot | Out-Null
    foreach ($dependency in ($allMetadata.packages | Sort-Object name, version)) {
        if ($dependency.name -eq 'pool-router' -or $resolvedIds -notcontains $dependency.id) { continue }
        if ($dependency.source -notlike 'registry+*') {
            throw 'Dependency licenses require review for non-registry sources.'
        }
        $notices.Add('')
        $notices.Add("$($dependency.name) $($dependency.version)")
        $notices.Add("License: $($dependency.license)")
        $notices.Add("Package: https://crates.io/crates/$($dependency.name)/$($dependency.version)")
        $sourceDir = [IO.Path]::GetFullPath((Split-Path -Parent $dependency.manifest_path))
        $allowedPrefix = $sourceDir.TrimEnd('\') + '\'
        $files = @(Get-ChildItem -LiteralPath $sourceDir -File -Recurse | Where-Object { $_.Name -match '^(LICENSE|LICENCE|COPYING|NOTICE|COPYRIGHT|UNLICENSE)([._-].*)?$' })
        if ($dependency.license_file) {
            $explicit = [IO.Path]::GetFullPath((Join-Path $sourceDir $dependency.license_file))
            if (-not $explicit.StartsWith($allowedPrefix, [StringComparison]::OrdinalIgnoreCase)) {
                throw 'A dependency license file is outside its public package directory.'
            }
            $files += Get-Item -LiteralPath $explicit
        }
        $files = @($files | Sort-Object FullName -Unique)
        if ($files.Count -gt 0) {
            $destination = Join-Path $licenseRoot "$($dependency.name)-$($dependency.version)"
            New-Item -ItemType Directory -Path $destination | Out-Null
            foreach ($file in $files) {
                $relative = $file.FullName.Substring($allowedPrefix.Length)
                $licenseDestination = Join-Path $destination $relative
                New-Item -ItemType Directory -Force -Path (Split-Path -Parent $licenseDestination) | Out-Null
                Copy-Item -LiteralPath $file.FullName -Destination $licenseDestination
            }
            $notices.Add("License texts: licenses/$($dependency.name)-$($dependency.version)/")
        }
        else {
            throw "No license text was found for $($dependency.name) $($dependency.version); resolve its attribution before packaging."
        }
    }
    [IO.File]::WriteAllLines((Join-Path $PayloadPath 'THIRD-PARTY-NOTICES.txt'), $notices, [Text.UTF8Encoding]::new($false))
}

Push-Location $repoRoot
try {
    $metadataText = & cargo metadata --no-deps --format-version 1 --locked
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed.' }
    $metadata = $metadataText | ConvertFrom-Json
    $package = @($metadata.packages | Where-Object { $_.name -eq 'pool-router' })
    if ($package.Count -ne 1) { throw 'Expected exactly one pool-router package.' }
    $version = [string]$package[0].version
    if ($version -notmatch '^\d+\.\d+\.\d+$') { throw 'The installer requires a numeric Cargo release version.' }
    if ($ReleaseTag -and $ReleaseTag -cne "v$version") {
        throw "Release tag $ReleaseTag does not match Cargo package version v$version."
    }
    if (-not $SkipBuild) {
        & cargo build --release --locked --target $target --bin pool-router --bin pool-router-launcher
        if ($LASTEXITCODE -ne 0) { throw 'The Windows release build failed.' }
    }
    $binaryDir = Join-Path ([string]$metadata.target_directory) "$target\release"
    foreach ($name in @('pool-router.exe', 'pool-router-launcher.exe')) {
        Assert-X64Executable (Join-Path $binaryDir $name)
    }
    $binaryVersion = & (Join-Path $binaryDir 'pool-router.exe') --version
    if ($LASTEXITCODE -ne 0 -or $binaryVersion -cne "Pool Router $version") {
        throw 'The built service version does not match Cargo metadata.'
    }

    $compiler = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if ($null -ne $compiler) { $iscc = $compiler.Source }
    else {
        $iscc = Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'
        if (-not (Test-Path -LiteralPath $iscc -PathType Leaf)) {
            throw 'Install Inno Setup 6 from https://jrsoftware.org/isinfo.php, then run this script again.'
        }
    }
    New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
    $payload = Join-Path ([IO.Path]::GetTempPath()) ('pool-router-package-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $payload | Out-Null
    try {
        foreach ($name in @('pool-router.exe', 'pool-router-launcher.exe')) {
            Copy-Item -LiteralPath (Join-Path $binaryDir $name) -Destination $payload
        }
        Copy-Item -LiteralPath (Join-Path $repoRoot 'LICENSE') -Destination (Join-Path $payload 'LICENSE.txt')
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'maintenance.ps1') -Destination $payload
        Copy-Item -LiteralPath (Join-Path $repoRoot 'assets/PoolRouter.ico') -Destination $payload
        Write-DependencyLicenses $payload
        $expected = @('LICENSE.txt', 'PoolRouter.ico', 'THIRD-PARTY-NOTICES.txt', 'licenses', 'maintenance.ps1', 'pool-router-launcher.exe', 'pool-router.exe') | Sort-Object
        $actual = @(Get-ChildItem -LiteralPath $payload | Select-Object -ExpandProperty Name | Sort-Object)
        if (@(Compare-Object $expected $actual).Count -ne 0) { throw 'The package allowlist did not match.' }
        & $iscc "/DAppVersion=$version" "/DPayloadDir=$payload" "/DOutputDir=$OutputDir" (Join-Path $PSScriptRoot 'installer.iss')
        if ($LASTEXITCODE -ne 0) { throw 'Inno Setup compilation failed.' }
        $installer = Join-Path $OutputDir 'PoolRouter-Windows-x64-Setup.exe'
        if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) { throw 'Installer output is missing.' }
        $hash = (Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash.ToLowerInvariant()
        [IO.File]::WriteAllText((Join-Path $OutputDir 'PoolRouter-Windows-x64-Setup.exe.sha256'), "$hash  PoolRouter-Windows-x64-Setup.exe`n", [Text.UTF8Encoding]::new($false))
        Write-Host "Built Pool Router $version for $target."
        Write-Host "Installer: $installer"
    }
    finally {
        Remove-Item -LiteralPath $payload -Recurse -Force
    }
}
finally {
    Pop-Location
    [Environment]::SetEnvironmentVariable($flagName, $originalFlags, 'Process')
}
