# Windows PowerShell 5.1 compatible. This file is also used by the uninstaller.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('Stop', 'Uninstall')]
    [string]$Action,
    [Parameter(Mandatory = $true)]
    [string]$InstallDir
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

try {
    $installRoot = [IO.Path]::GetFullPath($InstallDir).TrimEnd('\')
    $servicePath = Join-Path $installRoot 'pool-router.exe'
    $launcherPath = Join-Path $installRoot 'pool-router-launcher.exe'
    if ($Action -eq 'Uninstall') {
        if ([string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
            throw 'The per-user application data path is unavailable.'
        }
        $stateDir = Join-Path $env:LOCALAPPDATA 'Pool Router'
        $hasBackup = (Test-Path -LiteralPath (Join-Path $stateDir 'integration-backup.json')) -or
            (Test-Path -LiteralPath (Join-Path $stateDir 'cc-monitor-backup.json'))
        if ($hasBackup) {
            if (-not (Test-Path -LiteralPath $servicePath -PathType Leaf)) { exit 20 }
            # Restoration rejects changes to fields that Pool Router no longer owns.
            # Do this before stopping anything, so a conflict leaves a working route.
            $restoreInfo = [Diagnostics.ProcessStartInfo]::new()
            $restoreInfo.FileName = $servicePath
            $restoreInfo.Arguments = 'restore --config "' + (Join-Path $stateDir 'config.json') + '"'
            $restoreInfo.UseShellExecute = $false
            $restoreInfo.CreateNoWindow = $true
            $restoreInfo.RedirectStandardOutput = $true
            $restoreInfo.RedirectStandardError = $true
            $restoreProcess = [Diagnostics.Process]::new()
            $restoreProcess.StartInfo = $restoreInfo
            try {
                if (-not $restoreProcess.Start()) { exit 20 }
                $stdout = $restoreProcess.StandardOutput.ReadToEndAsync()
                $stderr = $restoreProcess.StandardError.ReadToEndAsync()
                $restoreProcess.WaitForExit()
                # Do not persist command output or credential-bearing configuration.
                $null = $stdout.GetAwaiter().GetResult()
                $null = $stderr.GetAwaiter().GetResult()
                if ($restoreProcess.ExitCode -ne 0) { exit 20 }
            }
            catch { exit 20 }
            finally { $restoreProcess.Dispose() }
        }
    }

    # Never taskkill /IM or stop by process name alone. Recheck the live path after
    # obtaining the PID to avoid acting on a stale process listing.
    # Stop the launcher first so it cannot start another service during replacement.
    foreach ($ownedPath in @($launcherPath, $servicePath)) {
        $imageName = [IO.Path]::GetFileName($ownedPath)
        $candidates = Get-CimInstance Win32_Process -Filter "Name='$imageName'"
        foreach ($candidate in $candidates) {
            if (-not $candidate.ExecutablePath) { continue }
            $candidatePath = [IO.Path]::GetFullPath($candidate.ExecutablePath)
            if ($ownedPath -ine $candidatePath) { continue }
            $process = Get-Process -Id $candidate.ProcessId -ErrorAction SilentlyContinue
            if ($null -eq $process) { continue }
            try {
                if ($process.HasExited) { continue }
                if ($ownedPath -ine [IO.Path]::GetFullPath($process.Path)) { continue }
                $process.Kill()
                if (-not $process.WaitForExit(10000)) {
                    throw 'An installed Pool Router process did not stop.'
                }
            }
            finally {
                $process.Dispose()
            }
        }
    }
    exit 0
}
catch {
    # Keep the message generic because PowerShell errors can contain private paths.
    [Console]::Error.WriteLine('Pool Router maintenance did not complete. Your user data has been preserved.')
    exit 1
}
