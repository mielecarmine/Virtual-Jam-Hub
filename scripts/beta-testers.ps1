<#
.SYNOPSIS
    Runner e analizzatore autonomo dei Beta-Tester per Virtual Jam Hub (Windows / PowerShell).
.DESCRIPTION
    Avvia una squadra di processi indipendenti `jam-client --synthetic` (e un `jam-server` locale
    se -Server non e' specificato) con profili di rete eterogenei (Fibra, VDSL, Wi-Fi, 4G),
    dimensioni buffer differenti (64, 128, 256 frame) e supporto `--bind-ip` multi-interfaccia.
    Raccoglie e valida i log `telemetry-*.txt` a 27 metriche producendo `report.md` e `summary.json`.
#>
param(
    [string]$Server = "",
    [int]$Seconds = 8,
    [ValidateSet("standard", "churn", "stress")]
    [string]$Preset = "standard",
    [string]$BindIps = "",
    [string]$OutputDir = "",
    [switch]$DebugBuild,
    [switch]$NoBuild
)

$ErrorActionPreference = "Stop"
$Workspace = Split-Path -Parent $PSScriptRoot
$TargetDir = Join-Path $Workspace "target"
$ProfileDir = if ($DebugBuild) { "debug" } else { "release" }

if (-not $NoBuild) {
    $BuildArgs = @("build", "--workspace", "--features", "jam-client/telemetry-log")
    if (-not $DebugBuild) {
        $BuildArgs += "--release"
    }
    Write-Host "[beta-testers] Compilazione workspace: cargo $($BuildArgs -join ' ')"
    & cargo @BuildArgs
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build fallito con codice $LASTEXITCODE"
    }
}

$ClientBin = Join-Path (Join-Path $TargetDir $ProfileDir) "jam-client.exe"
$ServerBin = Join-Path (Join-Path $TargetDir $ProfileDir) "jam-server.exe"
if (-not (Test-Path $ClientBin)) {
    throw "Binario jam-client non trovato in $ClientBin"
}

$Stamp = Get-Date -Format "yyyyMMdd-HHmmss"
if ([string]::IsNullOrWhiteSpace($OutputDir)) {
    $OutPath = Join-Path (Join-Path $Workspace "logs") "beta-$Stamp"
} else {
    $OutPath = [System.IO.Path]::GetFullPath($OutputDir)
}
New-Item -ItemType Directory -Force -Path $OutPath | Out-Null

$IpList = @()
if (-not [string]::IsNullOrWhiteSpace($BindIps)) {
    $IpList = $BindIps.Split(",") | ForEach-Object { $_.Trim() } | Where-Object { $_ -ne "" }
}

$Profiles = @(
    [ordered]@{
        name = "tester-1-fibra-leader"
        role = "Tempo Leader su Fibra/Ethernet (Buffer 64, 120 BPM)"
        buffer_frames = 64
        jitter_chunks = 3
        delay_ms = 3
        jitter_ms = 1
        loss = 0.0
        metronome_bpm = 120
        bind_ip = $null
        duration_s = $Seconds
        reconnect_cycles = 1
    },
    [ordered]@{
        name = "tester-2-dsl-128"
        role = "Musicista Home Broadband / VDSL (Buffer 128, Jitter 4ms)"
        buffer_frames = 128
        jitter_chunks = 3
        delay_ms = 10
        jitter_ms = 4
        loss = 0.002
        metronome_bpm = $null
        bind_ip = $null
        duration_s = $Seconds
        reconnect_cycles = 1
    },
    [ordered]@{
        name = "tester-3-wifi-256"
        role = "Musicista Wi-Fi Congestionato (Buffer 256, Jitter 12ms, Loss 1.5%)"
        buffer_frames = 256
        jitter_chunks = 4
        delay_ms = 15
        jitter_ms = 12
        loss = 0.015
        metronome_bpm = $null
        bind_ip = $null
        duration_s = $Seconds
        reconnect_cycles = 1
    },
    [ordered]@{
        name = "tester-4-mobile-4g"
        role = "Musicista Hotspot 4G/LTE (Buffer 128, Jitter 16ms, Loss 2.0%)"
        buffer_frames = 128
        jitter_chunks = 6
        delay_ms = 22
        jitter_ms = 16
        loss = 0.02
        metronome_bpm = $null
        bind_ip = $null
        duration_s = $Seconds
        reconnect_cycles = 1
    }
)

if ($Preset -eq "churn") {
    $CycleDur = [Math]::Max(3, [int]($Seconds / 2))
    $Profiles[3].role += " + Reconnect Churn (2 cicli)"
    $Profiles[3].duration_s = $CycleDur
    $Profiles[3].reconnect_cycles = 2
} elseif ($Preset -eq "stress") {
    $Profiles += @(
        [ordered]@{
            name = "tester-5-studio-64"
            role = "Seconda postazione Studio Low-Latency (Buffer 64)"
            buffer_frames = 64
            jitter_chunks = 2
            delay_ms = 4
            jitter_ms = 1
            loss = 0.0
            metronome_bpm = $null
            bind_ip = $null
            duration_s = $Seconds
            reconnect_cycles = 1
        },
        [ordered]@{
            name = "tester-6-wifi-heavy"
            role = "Wi-Fi degradato (Buffer 256, Jitter 20ms, Loss 3.0%)"
            buffer_frames = 256
            jitter_chunks = 6
            delay_ms = 25
            jitter_ms = 20
            loss = 0.03
            metronome_bpm = $null
            bind_ip = $null
            duration_s = $Seconds
            reconnect_cycles = 1
        }
    )
}

for ($i = 0; $i -lt $Profiles.Count; $i++) {
    if ($IpList.Count -gt 0) {
        $Profiles[$i].bind_ip = $IpList[$i % $IpList.Count]
    }
}

$LocalServerProc = $null
$ServerAddr = $Server
if ([string]::IsNullOrWhiteSpace($ServerAddr)) {
    if (-not (Test-Path $ServerBin)) {
        throw "Binario jam-server non trovato in $ServerBin"
    }
    $Udp = New-Object System.Net.Sockets.UdpClient(0)
    $FreePort = $Udp.Client.LocalEndPoint.Port
    $Udp.Close()
    $ServerAddr = "127.0.0.1:$FreePort"
    $SrvOut = Join-Path $OutPath "server-stdout.txt"
    $SrvErr = Join-Path $OutPath "server-stderr.txt"
    $LocalServerProc = Start-Process -FilePath $ServerBin -ArgumentList @($ServerAddr) `
        -WorkingDirectory $Workspace -RedirectStandardOutput $SrvOut -RedirectStandardError $SrvErr `
        -NoNewWindow -PassThru
    Start-Sleep -Milliseconds 300
}

Write-Host "[beta-testers] Avvio di $($Profiles.Count) agenti beta-tester su $ServerAddr -> $OutPath"
$StartTime = [System.Diagnostics.Stopwatch]::StartNew()
$ActiveRuns = @()

try {
    $MaxCycles = ($Profiles | ForEach-Object { $_.reconnect_cycles } | Measure-Object -Maximum).Maximum
    for ($cycle = 0; $cycle -lt $MaxCycles; $cycle++) {
        $RunningProcs = @()
        foreach ($p in $Profiles) {
            if ($cycle -eq 0) {
                $TesterDir = Join-Path $OutPath $p.name
                New-Item -ItemType Directory -Force -Path $TesterDir | Out-Null
                Remove-Item -Path (Join-Path $TesterDir "telemetry-*.txt") -Force -ErrorAction SilentlyContinue
                $ActiveRuns += [ordered]@{
                    profile = $p
                    dir = $TesterDir
                    exit_code = 0
                    telemetry_logs = @()
                }
            }
            if ($cycle -ge $p.reconnect_cycles) {
                continue
            }
            $Entry = $ActiveRuns | Where-Object { $_.profile.name -eq $p.name } | Select-Object -First 1
            $TesterDir = $Entry.dir
            $LossStr = $p.loss.ToString([System.Globalization.CultureInfo]::InvariantCulture)
            $ClientArgs = @(
                "--server", $ServerAddr,
                "--nickname", $p.name,
                "--synthetic",
                "--buffer-frames", "$($p.buffer_frames)",
                "--jitter-chunks", "$($p.jitter_chunks)",
                "--simulate-delay-ms", "$($p.delay_ms)",
                "--simulate-jitter-ms", "$($p.jitter_ms)",
                "--simulate-loss", $LossStr,
                "--seconds", "$($p.duration_s)",
                "--telemetry-dir", $TesterDir
            )
            if ($null -ne $p.metronome_bpm) {
                $ClientArgs += @("--metronome-bpm", "$($p.metronome_bpm)")
            }
            if ($null -ne $p.bind_ip) {
                $ClientArgs += @("--bind-ip", "$($p.bind_ip)")
            }
            $OutFile = Join-Path $TesterDir "stdout-cycle$($cycle + 1).txt"
            $ErrFile = Join-Path $TesterDir "stderr-cycle$($cycle + 1).txt"
            $Proc = Start-Process -FilePath $ClientBin -ArgumentList $ClientArgs `
                -WorkingDirectory $Workspace -RedirectStandardOutput $OutFile -RedirectStandardError $ErrFile `
                -NoNewWindow -PassThru
            $RunningProcs += [PSCustomObject]@{ Entry = $Entry; Proc = $Proc }
        }
        foreach ($item in $RunningProcs) {
            $item.Proc.WaitForExit()
            if ($item.Proc.ExitCode -ne 0) {
                $item.Entry.exit_code = $item.Proc.ExitCode
            }
        }
    }
} finally {
    if ($null -ne $LocalServerProc -and -not $LocalServerProc.HasExited) {
        Stop-Process -Id $LocalServerProc.Id -Force -ErrorAction SilentlyContinue
    }
}

$StartTime.Stop()
$ElapsedS = $StartTime.Elapsed.TotalSeconds

function Parse-OptionalFloat([string]$Token) {
    if ($Token -eq "N/A" -or [string]::IsNullOrWhiteSpace($Token)) {
        return $null
    }
    $val = 0.0
    if ([double]::TryParse($Token, [System.Globalization.NumberStyles]::Float, [System.Globalization.CultureInfo]::InvariantCulture, [ref]$val)) {
        return $val
    }
    return $null
}

function Parse-TelemetryFile([string]$FilePath) {
    $Lines = Get-Content -Path $FilePath -Encoding UTF8
    $Header = [ordered]@{}
    $RowCount = 0
    $Summary = [ordered]@{}
    $DroppedSamples = $null
    $Mode = "header"

    foreach ($rawLine in $Lines) {
        $Line = $rawLine.Trim()
        if ([string]::IsNullOrWhiteSpace($Line)) { continue }
        if ($Line.StartsWith("telemetry_dropped_samples=")) {
            $DroppedSamples = [int]($Line.Split("=", 2)[1])
            continue
        }
        if ($Line -eq "Summary") {
            $Mode = "summary_header"
            continue
        }
        if ($Mode -eq "header") {
            if ($Line.StartsWith("elapsed_s`t")) {
                $Mode = "rows"
            } elseif ($Line.Contains("=") -and -not $Line.StartsWith("interval_ms=") -and -not $Line.StartsWith("counters=") -and -not $Line.StartsWith("latency=")) {
                $Parts = $Line.Split("=", 2)
                $Header[$Parts[0]] = $Parts[1]
            }
        } elseif ($Mode -eq "rows") {
            $RowCount++
        } elseif ($Mode -eq "summary_header") {
            if ($Line.StartsWith("metric`t")) {
                $Mode = "summary_rows"
            }
        } elseif ($Mode -eq "summary_rows") {
            $Parts = $Line.Split("`t")
            if ($Parts.Count -eq 6) {
                $Summary[$Parts[0]] = [ordered]@{
                    samples = [int]$Parts[1]
                    min_val = Parse-OptionalFloat $Parts[2]
                    avg_val = Parse-OptionalFloat $Parts[3]
                    p95_val = Parse-OptionalFloat $Parts[4]
                    max_val = Parse-OptionalFloat $Parts[5]
                }
            }
        }
    }
    return [ordered]@{
        file = $FilePath
        header = $Header
        row_count = $RowCount
        dropped_samples = $DroppedSamples
        has_summary = ($Summary.Count -gt 0)
        summary = $Summary
    }
}

foreach ($entry in $ActiveRuns) {
    $Files = Get-ChildItem -Path $entry.dir -Filter "telemetry-*.txt" | Sort-Object Name
    $Logs = @()
    foreach ($f in $Files) {
        $Logs += Parse-TelemetryFile $f.FullName
    }
    $entry.telemetry_logs = $Logs
}

$ExpectedPeers = [Math]::Max(1, $Profiles.Count - 1)
$AllExitedZero = @($ActiveRuns | Where-Object { $_.exit_code -ne 0 -or @($_.telemetry_logs).Count -eq 0 }).Count -eq 0
$AllSummariesOk = $true
$PeersOk = $true
$PeerDetails = @()
$RxDrops = 0
$CallbacksOk = $true
$StableClean = $true
$StableNotes = @()

foreach ($r in $ActiveRuns) {
    $MaxSeen = 0
    foreach ($log in $r.telemetry_logs) {
        if (-not $log.has_summary -or $log.dropped_samples -ne 0) {
            $AllSummariesOk = $false
        }
        $rp = $log.summary["remote_peers"].max_val
        if ($null -ne $rp -and $rp -gt $MaxSeen) { $MaxSeen = [int]$rp }
        $rxd = $log.summary["rx_push_dropped"].max_val
        if ($null -ne $rxd) { $RxDrops += [int]$rxd }
        $outCb = $log.summary["output_callbacks"].max_val
        if ($null -eq $outCb -or $outCb -le 0) { $CallbacksOk = $false }
        if ($r.profile.loss -eq 0.0 -and $r.profile.jitter_ms -le 2) {
            $uf = [int]($log.summary["output_underflows"].max_val)
            $far = [int]($log.summary["late_dropped_far"].max_val)
            $StableNotes += "$($r.profile.name): underflows=$uf, late_far=$far"
            if ($uf -gt 0 -or $far -gt 0) { $StableClean = $false }
        }
    }
    $PeerDetails += "$($r.profile.name): max $MaxSeen peer remoti"
    if ($MaxSeen -lt $ExpectedPeers) { $PeersOk = $false }
}

$Gates = @(
    [ordered]@{
        gate = "1. Avvio & Completamento Sessioni"
        status = if ($AllExitedZero) { "PASS" } else { "FAIL" }
        details = "Tutti i processi client hanno completato la sessione con exit code 0 e generato i log."
    },
    [ordered]@{
        gate = "2. Integrita' Logger Telemetria (JAM-20)"
        status = if ($AllSummariesOk) { "PASS" } else { "FAIL" }
        details = "Summary finale presente e telemetry_dropped_samples=0 su tutti i file."
    },
    [ordered]@{
        gate = "3. Handshake UDP & Discovery Stanza"
        status = if ($PeersOk) { "PASS" } else { "FAIL" }
        details = ($PeerDetails -join ", ")
    },
    [ordered]@{
        gate = "4. Continuita' Motore Audio & Code SPSC (JAM-24)"
        status = if ($RxDrops -eq 0 -and $CallbacksOk) { "PASS" } else { "FAIL" }
        details = "rx_push_dropped totali=$RxDrops, callback output attive=$CallbacksOk."
    },
    [ordered]@{
        gate = "5. Assenza Glitch su Peer Stabili (JAM-30)"
        status = if ($StableClean) { "PASS" } else { "WARN" }
        details = ($StableNotes -join "; ")
    }
)

function Fmt-Num($Val) {
    if ($null -eq $Val) { return "N/A" }
    return ([double]$Val).ToString("F2", [System.Globalization.CultureInfo]::InvariantCulture)
}

$MdLines = @(
    "# Virtual Jam — Report Beta-Testing Autonomo Pre-Release",
    "",
    "- **Server UDP**: ``$ServerAddr``",
    "- **Profilo Suite**: ``$Preset`` ($($ActiveRuns.Count) agenti beta-tester)",
    "- **Durata Totale**: ``$(Fmt-Num $ElapsedS) s``",
    "",
    "## 1. Esito Quality Gates Pre-Release",
    "",
    "| Gate | Esito | Dettagli |",
    "| --- | --- | --- |"
)

foreach ($g in $Gates) {
    $MdLines += "| $($g.gate) | **$($g.status)** | $($g.details) |"
}

$MdLines += @(
    "",
    "## 2. Riepilogo Telemetria per Agente Beta-Tester",
    "",
    "| Agente | Buffer | Rete Simulata (Delay / Jitter / Loss) | RTT avg/p95 (ms) | Jitter p95 (ms) | Loss max (%) | Buffer p95 (ms) | E2E One-Way avg/p95 (ms) | Headroom avg (ms) | PLC max | Underflows | Gap Worker p95 (ms) |",
    "| --- | ---: | --- | --- | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: |"
)

foreach ($r in $ActiveRuns) {
    $p = $r.profile
    $LossPct = ([double]$p.loss * 100.0).ToString("F1", [System.Globalization.CultureInfo]::InvariantCulture)
    $NetDesc = "$($p.delay_ms)ms / $($p.jitter_ms)ms / $LossPct% (c=$($p.jitter_chunks))"
    if ($null -ne $p.bind_ip) {
        $NetDesc += " @ ``$($p.bind_ip)``"
    }
    for ($idx = 0; $idx -lt $r.telemetry_logs.Count; $idx++) {
        $log = $r.telemetry_logs[$idx]
        $s = $log.summary
        $NameLabel = if ($r.telemetry_logs.Count -eq 1) { $p.name } else { "$($p.name) (#$($idx + 1))" }
        $MdLines += "| ``$NameLabel`` | $($p.buffer_frames) | $NetDesc | $(Fmt-Num $s['rtt_ms'].avg_val) / $(Fmt-Num $s['rtt_ms'].p95_val) | $(Fmt-Num $s['jitter_ms'].p95_val) | $(Fmt-Num $s['loss_percent'].max_val) | $(Fmt-Num $s['buffer_ms'].p95_val) | $(Fmt-Num $s['total_one_way_ms'].avg_val) / $(Fmt-Num $s['total_one_way_ms'].p95_val) | $(Fmt-Num $s['headroom_ms'].avg_val) | $([int]($s['plc_frames'].max_val)) | $([int]($s['output_underflows'].max_val)) | $(Fmt-Num $s['worker_max_gap_ms'].p95_val) |"
    }
}

$MdLines += @("", "## 3. File Log Generati", "")
foreach ($r in $ActiveRuns) {
    foreach ($log in $r.telemetry_logs) {
        $MdLines += "- ``$($r.profile.name)``: ``$($log.file)`` ($($log.row_count) campioni)"
    }
}

$ReportMd = ($MdLines -join "`n") + "`n"
$ReportPath = Join-Path $OutPath "report.md"
$SummaryPath = Join-Path $OutPath "summary.json"
[System.IO.File]::WriteAllText($ReportPath, $ReportMd, [System.Text.Encoding]::UTF8)

$SummaryObj = [ordered]@{
    server = $ServerAddr
    preset = $Preset
    elapsed_s = $ElapsedS
    gates = $Gates
    testers = $ActiveRuns
}
[System.IO.File]::WriteAllText($SummaryPath, ($SummaryObj | ConvertTo-Json -Depth 10), [System.Text.Encoding]::UTF8)

Write-Host ""
Write-Host $ReportMd
Write-Host "[beta-testers] Report scritto in $ReportPath"
Write-Host "[beta-testers] JSON summary scritto in $SummaryPath"

if (@($Gates | Where-Object { $_.status -eq "FAIL" }).Count -gt 0) {
    exit 1
}
exit 0
