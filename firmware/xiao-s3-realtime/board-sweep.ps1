# The interrupt-path fixes, A/B on the board: five builds of the same firmware,
# each flashed and run to its report, each log kept.
#
#     cd rusty_rtos_kernel\firmware\xiao-s3-realtime
#     powershell -ExecutionPolicy Bypass -File .\board-sweep.ps1          # COM4
#     powershell -ExecutionPolicy Bypass -File .\board-sweep.ps1 -Port COM5
#
# Every run carries `decompose`, so every log has the segment table. The kernel
# fix (queue_take's single resolve) is in all five, being a kernel change; the
# baseline here is therefore the earlier decompose run plus that fix alone.
#
# Logs: .\sweep-logs\<n>-<name>.log. Each run takes about 25 s after the flash
# and is stopped at its closing "done --" line (or after 120 s).

param([string]$Port = "COM4")

$ErrorActionPreference = "Stop"
$here = $PSScriptRoot
$logs = Join-Path $here "sweep-logs"
New-Item -ItemType Directory -Force $logs | Out-Null
$elf = Join-Path $here "target\xtensa-esp32s3-none-elf\release\xiao-s3-realtime"

$runs = @(
    @{ name = "1-baseline";       args = @("--features", "decompose") },
    @{ name = "2-switch-in-trap"; args = @("--features", "decompose,switch-in-trap") },
    @{ name = "3-no-fp-save";     args = @("--no-default-features", "--features", "decompose") },
    @{ name = "4-iram";           args = @("--features", "decompose,iram") },
    @{ name = "5-all";            args = @("--no-default-features", "--features", "decompose,switch-in-trap,iram") }
)

Push-Location $here
try {
    foreach ($r in $runs) {
        Write-Host ""
        Write-Host "=== $($r.name): cargo +esp build --release $($r.args -join ' ')"
        & cargo +esp build --release @($r.args)
        if ($LASTEXITCODE -ne 0) { throw "build failed: $($r.name)" }

        $log = Join-Path $logs "$($r.name).log"
        Remove-Item -ErrorAction SilentlyContinue $log, "$log.err"
        $p = Start-Process -FilePath "espflash" -NoNewWindow -PassThru `
            -ArgumentList @("flash", "--port", $Port, "--non-interactive", "--monitor", "`"$elf`"") `
            -RedirectStandardOutput $log -RedirectStandardError "$log.err"
        $deadline = (Get-Date).AddSeconds(120)
        while (-not $p.HasExited -and (Get-Date) -lt $deadline) {
            Start-Sleep -Seconds 2
            if ((Test-Path $log) -and (Select-String -Path $log -Pattern "^done --" -Quiet)) { break }
        }
        if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
        Start-Sleep -Seconds 2

        $result = Select-String -Path $log -Pattern "^RESULT:" | Select-Object -First 1
        $clock = Select-String -Path $log -Pattern "^clock " | Select-Object -First 1
        Write-Host "    $($clock.Line)"
        if ($result) { Write-Host "    $($result.Line)" } else { Write-Host "    no RESULT line -- see $log and $log.err" }
    }
} finally {
    Pop-Location
}
Write-Host ""
Write-Host "logs in $logs"
