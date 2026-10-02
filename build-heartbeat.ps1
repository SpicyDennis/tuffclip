# Runs a command and prints a "still working" line every 15 s, so a long silent
# step (the final LTO link of the clipr crate) doesn't look like a hang.
# Usage: powershell -File build-heartbeat.ps1 <exe> <args...>
$exe = $args[0]
$rest = @($args | Select-Object -Skip 1)
$sw = [Diagnostics.Stopwatch]::StartNew()
$p = Start-Process -FilePath $exe -ArgumentList $rest -NoNewWindow -PassThru
$null = $p.Handle  # keep the handle so ExitCode is readable after exit
while (-not $p.WaitForExit(15000)) {
    $t = $sw.Elapsed
    Write-Host ("`n  ... still working ({0}m {1:00}s elapsed). Silent steps like the final link are normal." -f [int][math]::Floor($t.TotalMinutes), $t.Seconds)
}
exit $p.ExitCode
