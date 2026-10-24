# Exercises a real installer on a disposable Windows account / CI runner.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Installer
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($env:OS -ne 'Windows_NT') { throw 'The installer smoke test requires Windows.' }
if (-not $env:CI) { throw 'Run this installer smoke test only on a disposable CI runner.' }
$Installer = (Resolve-Path -LiteralPath $Installer).Path
$scratch = Join-Path ([IO.Path]::GetTempPath()) ('pool-router-smoke-' + [guid]::NewGuid().ToString('N'))
$installDir = Join-Path $scratch 'Installed Pool Router'
$profileDir = Join-Path $scratch 'User Profile'
$localDir = Join-Path $profileDir 'AppData\Local'
$roamingDir = Join-Path $profileDir 'AppData\Roaming'
$stateDir = Join-Path $localDir 'Pool Router'
$configPath = Join-Path $stateDir 'config.json'
$codexDir = Join-Path $profileDir '.codex'
$codexPath = Join-Path $codexDir 'config.toml'
$service = Join-Path $installDir 'pool-router.exe'
$launcher = Join-Path $installDir 'pool-router-launcher.exe'
$maintenance = Join-Path $installDir 'maintenance.ps1'
$uninstaller = Join-Path $installDir 'unins000.exe'
$otherProcess = $null
$otherService = Join-Path $scratch 'Other Installation\pool-router.exe'
$oldEnvironment = @{}
foreach ($name in @('LOCALAPPDATA', 'APPDATA', 'USERPROFILE', 'CODEX_HOME')) {
    $oldEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}

function Invoke-Executable([string]$FilePath, [string[]]$Arguments) {
    $quoted = @($Arguments | ForEach-Object {
        if ($_ -match '"') { throw 'Unexpected quote in a smoke test argument.' }
        '"' + $_ + '"'
    })
    # Start-Process -Wait also waits for descendants on Windows and would wait
    # forever for the background service created by the launcher.
    $process = Start-Process -FilePath $FilePath -ArgumentList ($quoted -join ' ') -PassThru -WindowStyle Hidden
    try {
        if (-not $process.WaitForExit(180000)) { throw 'A smoke test executable timed out.' }
        return $process.ExitCode
    }
    finally { $process.Dispose() }
}

function Invoke-Checked([string]$FilePath, [string[]]$Arguments) {
    $exitCode = Invoke-Executable $FilePath $Arguments
    if ($exitCode -ne 0) { throw "A smoke test executable failed with exit code $exitCode." }
}

function Wait-Healthy([string]$BaseUrl) {
    for ($attempt = 0; $attempt -lt 60; $attempt++) {
        try {
            $health = Invoke-RestMethod -Uri "$BaseUrl/health" -TimeoutSec 1
            if ($health.service -eq 'pool-router' -and $health.status -eq 'ok') { return }
        }
        catch { }
        Start-Sleep -Milliseconds 250
    }
    throw 'The installed service did not become healthy.'
}

try {
    foreach ($directory in @($localDir, $roamingDir, $stateDir, $codexDir)) {
        New-Item -ItemType Directory -Force -Path $directory | Out-Null
    }
    [Environment]::SetEnvironmentVariable('LOCALAPPDATA', $localDir, 'Process')
    [Environment]::SetEnvironmentVariable('APPDATA', $roamingDir, 'Process')
    [Environment]::SetEnvironmentVariable('USERPROFILE', $profileDir, 'Process')
    # Clear a runner-specific override so Codex discovery uses the isolated profile.
    [Environment]::SetEnvironmentVariable('CODEX_HOME', $null, 'Process')

    # Reserve an unused local port, then release it immediately before launch.
    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = $listener.LocalEndpoint.Port
    $listener.Stop()
    $baseUrl = "http://127.0.0.1:$port"
    $token = 'ci_' + [guid]::NewGuid().ToString('N')
    $config = @{
        source_mode = 'manual'
        upstream_base_url = 'http://127.0.0.1:9/v1'
        listen = "127.0.0.1:$port"
        biwrite_dir = (Join-Path $roamingDir 'app.biwrite.desktop')
        local_token = $token
        legacy_monitor_enabled = $false
        sync_paused = $true
        recording_enabled = $false
    } | ConvertTo-Json
    [IO.File]::WriteAllText($configPath, $config, [Text.UTF8Encoding]::new($false))
    $originalCodex = 'model = "fixture-model"' + "`n"
    [IO.File]::WriteAllText($codexPath, $originalCodex, [Text.UTF8Encoding]::new($false))

    # /DIR isolates application files; optional desktop/startup tasks stay disabled.
    Invoke-Checked $Installer @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-', "/DIR=$installDir", '/TASKS=')
    foreach ($name in @('pool-router.exe', 'pool-router-launcher.exe', 'LICENSE.txt', 'THIRD-PARTY-NOTICES.txt', 'maintenance.ps1', 'unins000.exe')) {
        if (-not (Test-Path -LiteralPath (Join-Path $installDir $name))) { throw "Missing installed file: $name" }
    }
    if (([IO.File]::ReadAllText($codexPath)) -cne $originalCodex) { throw 'Installation changed Codex configuration.' }
    Invoke-Checked $launcher @('--background', '--config', $configPath)
    Wait-Healthy $baseUrl
    $headers = @{ Authorization = "Bearer $token" }
    $state = Invoke-RestMethod -Uri "$baseUrl/api/state" -Headers $headers -TimeoutSec 5
    if ($null -eq $state) { throw 'The authenticated state API returned no data.' }
    $page = Invoke-WebRequest -Uri "$baseUrl/" -Headers $headers -UseBasicParsing -TimeoutSec 5
    if ($page.StatusCode -ne 200 -or $page.Content -notmatch '<html') { throw 'The dashboard did not load.' }
    # Exercise the actual launcher -> service open -> default browser path. This
    # verifies successful process handoff; it does not claim a visual browser test.
    Invoke-Checked $launcher @('--config', $configPath)
    Wait-Healthy $baseUrl

    # A second executable with the same name in a different directory must survive
    # update/uninstall of this installation. Both services use empty fixture pools.
    $otherDirectory = Split-Path -Parent $otherService
    New-Item -ItemType Directory -Path $otherDirectory | Out-Null
    Copy-Item -LiteralPath $service -Destination $otherService
    $otherListener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $otherListener.Start()
    $otherPort = $otherListener.LocalEndpoint.Port
    $otherListener.Stop()
    $otherConfig = $config | ConvertFrom-Json
    $otherConfig.listen = "127.0.0.1:$otherPort"
    $otherConfigPath = Join-Path $otherDirectory 'config.json'
    [IO.File]::WriteAllText($otherConfigPath, ($otherConfig | ConvertTo-Json -Depth 20), [Text.UTF8Encoding]::new($false))
    $otherProcess = Start-Process -FilePath $otherService -ArgumentList ('serve --config "' + $otherConfigPath + '"') -PassThru -WindowStyle Hidden
    Wait-Healthy "http://127.0.0.1:$otherPort"

    # Upgrade must preserve the saved configuration and allow a clean restart.
    $configBeforeUpgrade = (Get-FileHash -LiteralPath $configPath -Algorithm SHA256).Hash
    Invoke-Checked $Installer @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-', "/DIR=$installDir", '/TASKS=')
    if ($otherProcess.HasExited) { throw 'Upgrade stopped an unrelated installation.' }
    if ((Get-FileHash -LiteralPath $configPath -Algorithm SHA256).Hash -cne $configBeforeUpgrade) { throw 'Upgrade changed user configuration.' }
    Invoke-Checked $launcher @('--background', '--config', $configPath)
    Wait-Healthy $baseUrl

    # Test a conflict with a fake Codex configuration. A rejected restore must
    # preserve both the backup and the running service before uninstall removes files.
    Invoke-Checked $service @('connect', '--config', $configPath)
    $connectedCodex = [IO.File]::ReadAllText($codexPath)
    if ($connectedCodex -notmatch 'pool_router') { throw 'The fixture did not connect.' }
    $conflictedCodex = $connectedCodex.Replace($token, 'fixture-conflict-token-with-at-least-32-characters')
    if ($conflictedCodex -ceq $connectedCodex) { throw 'The conflict fixture did not change a managed field.' }
    [IO.File]::WriteAllText($codexPath, $conflictedCodex, [Text.UTF8Encoding]::new($false))
    $maintenanceExit = Invoke-Executable 'powershell.exe' @('-NoLogo', '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', $maintenance, '-Action', 'Uninstall', '-InstallDir', $installDir)
    if ($maintenanceExit -ne 20) { throw 'Conflicting restore did not abort uninstall maintenance.' }
    if (([IO.File]::ReadAllText($codexPath)) -cne $conflictedCodex) { throw 'Conflict handling overwrote Codex configuration.' }
    if (-not (Test-Path -LiteralPath (Join-Path $stateDir 'integration-backup.json'))) { throw 'Conflict handling removed its recovery backup.' }
    Wait-Healthy $baseUrl

    # Remove the synthetic conflict and let the real uninstaller restore its backup.
    [IO.File]::WriteAllText($codexPath, $connectedCodex, [Text.UTF8Encoding]::new($false))
    Invoke-Checked $uninstaller @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART')
    if ($otherProcess.HasExited) { throw 'Uninstall stopped an unrelated installation.' }
    if (Test-Path -LiteralPath $service) { throw 'Uninstall left the service executable installed.' }
    if (-not (Test-Path -LiteralPath $configPath)) { throw 'Uninstall removed user configuration.' }
    if (([IO.File]::ReadAllText($codexPath)) -cne $originalCodex) { throw 'Uninstall did not restore the original Codex fixture.' }
    Write-Host 'PASS: install, isolated service startup, health, authenticated dashboard, browser handoff, upgrade, process path isolation, conflict preservation and uninstall restoration.'
}
finally {
    if (Test-Path -LiteralPath $maintenance) {
        $null = Invoke-Executable 'powershell.exe' @('-NoLogo', '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', $maintenance, '-Action', 'Stop', '-InstallDir', $installDir)
    }
    if ($null -ne $otherProcess) {
        try {
            if (-not $otherProcess.HasExited -and $otherProcess.Path -ieq $otherService) {
                $otherProcess.Kill()
                $null = $otherProcess.WaitForExit(10000)
            }
        }
        finally { $otherProcess.Dispose() }
    }
    foreach ($name in $oldEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($name, $oldEnvironment[$name], 'Process')
    }
    # The scratch directory contains only generated fixtures. Never upload it.
    if (Test-Path -LiteralPath $scratch) { Remove-Item -LiteralPath $scratch -Recurse -Force -ErrorAction SilentlyContinue }
}
