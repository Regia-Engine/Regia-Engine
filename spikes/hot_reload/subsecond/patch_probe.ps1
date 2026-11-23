# Windows version of patch_probe.sh: starts `dx serve --hot-patch`, waits for the app, edits the
# game library crate's system body N times and measures save -> first output of the new body.
# Usage: powershell -File patch_probe.ps1 -Dx <path to dx.exe> [-Rounds 5]
param([string]$Dx = "dx", [int]$Rounds = 5)
$ErrorActionPreference = "Continue"
Set-Location $PSScriptRoot
$log = Join-Path $PSScriptRoot "dx-serve.log"
$err = Join-Path $PSScriptRoot "dx-serve.err.log"
Remove-Item -ErrorAction SilentlyContinue $log, $err
$env:PATCH_SECONDS = "900"
$p = Start-Process -PassThru -WindowStyle Hidden -FilePath $Dx `
    -ArgumentList "serve", "--hot-patch", "-p", "patch_host", "--interactive", "false", "--open", "false" `
    -RedirectStandardOutput $log -RedirectStandardError $err
$deadline = (Get-Date).AddMinutes(40)
while (-not (Select-String -Quiet -Path $log -Pattern "lib frame=" -ErrorAction SilentlyContinue)) {
    if ((Get-Date) -gt $deadline -or $p.HasExited) { "dx did not start the app"; Get-Content $log -Tail 30; Get-Content $err -Tail 30; exit 1 }
    Start-Sleep -Seconds 2
}
"app running"
$src = Join-Path $PSScriptRoot "patch_game\src\lib.rs"
$orig = Get-Content -Raw $src
for ($n = 2; $n -le $Rounds + 1; $n++) {
    $s = Get-Content -Raw $src
    $s = $s -replace 'const MARKER: &str = "[^"]*";', "const MARKER: &str = `"r$n`";"
    $s = $s -replace 'g\.value \+= [0-9]+;', "g.value += $n;"
    $t0 = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    [IO.File]::WriteAllText($src, $s)
    $hit = $null
    while (-not $hit) {
        Start-Sleep -Milliseconds 50
        $hit = Select-String -Path $log -Pattern "marker=r$n\s*$" | Select-Object -First 1
        if ([DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() - $t0 -gt 180000) { "round ${n}: TIMEOUT"; break }
    }
    if ($hit) {
        $t1 = [int64]([regex]::Match($hit.Line, 'PATCHPROBE (\d+)').Groups[1].Value)
        "round ${n}: save->visible $($t1 - $t0) ms | $([regex]::Match($hit.Line, 'frame=.*').Value)"
    }
    Start-Sleep -Seconds 3
}
[IO.File]::WriteAllText($src, $orig)
Start-Sleep -Seconds 5
Select-String -Path $log -Pattern "Hot-patching|patch-applied|rebuild|ERROR|error" | Select-Object -Last 20 | % { $_.Line }
Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
Get-Process patch_host -ErrorAction SilentlyContinue | Stop-Process -Force
