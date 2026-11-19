#!/usr/bin/env python3
"""Autonomous Beta-Tester Cohort Runner & Telemetry Analyzer for Virtual Jam Hub.

Spawns multiple independent `jam-client --synthetic` processes (and an optional local
`jam-server`) with heterogeneous network profiles, buffer sizes (64/128/256 frames),
and optional multi-interface `--bind-ip` routing. Collects and validates the 27-metric
`telemetry-*.txt` session logs against pre-release quality gates.
"""

from __future__ import annotations

import argparse
import dataclasses
from datetime import datetime
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time
from typing import Any


@dataclasses.dataclass
class TesterProfile:
    name: str
    role: str
    buffer_frames: int
    jitter_chunks: int
    delay_ms: int
    jitter_ms: int
    loss: float
    metronome_bpm: int | None = None
    bind_ip: str | None = None
    stagger_start_s: float = 0.0
    duration_s: int | None = None
    reconnect_cycles: int = 1


@dataclasses.dataclass
class MetricSummary:
    samples: int
    min_val: float | None
    avg_val: float | None
    p95_val: float | None
    max_val: float | None


def parse_float(token: str) -> float | None:
    if token == "N/A":
        return None
    try:
        return float(token)
    except ValueError:
        return None


def parse_telemetry_file(path: Path) -> dict[str, Any]:
    text = path.read_text(encoding="utf-8", errors="replace")
    lines = [line.strip() for line in text.splitlines() if line.strip()]
    header: dict[str, str] = {}
    rows: list[dict[str, float | None]] = []
    columns: list[str] = []
    summary: dict[str, MetricSummary] = {}
    dropped_samples: int | None = None

    mode = "header"
    for line in lines:
        if line.startswith("telemetry_dropped_samples="):
            try:
                dropped_samples = int(line.split("=", 1)[1])
            except ValueError:
                pass
            continue
        if line == "Summary":
            mode = "summary_header"
            continue
        if mode == "header":
            if line.startswith("elapsed_s\t"):
                columns = line.split("\t")
                mode = "rows"
            elif "=" in line and not line.startswith("interval_ms=") and not line.startswith("counters=") and not line.startswith("latency="):
                k, v = line.split("=", 1)
                header[k] = v
        elif mode == "rows":
            parts = line.split("\t")
            if len(parts) == len(columns):
                row = {col: parse_float(val) for col, val in zip(columns, parts)}
                rows.append(row)
        elif mode == "summary_header":
            if line.startswith("metric\t"):
                mode = "summary_rows"
        elif mode == "summary_rows":
            parts = line.split("\t")
            if len(parts) == 6:
                metric_name = parts[0]
                try:
                    samples = int(parts[1])
                except ValueError:
                    samples = 0
                summary[metric_name] = MetricSummary(
                    samples=samples,
                    min_val=parse_float(parts[2]),
                    avg_val=parse_float(parts[3]),
                    p95_val=parse_float(parts[4]),
                    max_val=parse_float(parts[5]),
                )

    return {
        "file": str(path),
        "header": header,
        "row_count": len(rows),
        "dropped_samples": dropped_samples,
        "has_summary": bool(summary),
        "summary": {k: dataclasses.asdict(v) for k, v in summary.items()},
    }


def build_profiles(
    preset: str,
    total_seconds: int,
    bind_ips: list[str],
) -> list[TesterProfile]:
    profiles = [
        TesterProfile(
            name="tester-1-fibra-leader",
            role="Tempo Leader su Fibra/Ethernet (Buffer 64, 120 BPM)",
            buffer_frames=64,
            jitter_chunks=3,
            delay_ms=3,
            jitter_ms=1,
            loss=0.0,
            metronome_bpm=120,
        ),
        TesterProfile(
            name="tester-2-dsl-128",
            role="Musicista Home Broadband / VDSL (Buffer 128, Jitter 4ms)",
            buffer_frames=128,
            jitter_chunks=3,
            delay_ms=10,
            jitter_ms=4,
            loss=0.002,
        ),
        TesterProfile(
            name="tester-3-wifi-256",
            role="Musicista Wi-Fi Congestionato (Buffer 256, Jitter 12ms, Loss 1.5%)",
            buffer_frames=256,
            jitter_chunks=4,
            delay_ms=15,
            jitter_ms=12,
            loss=0.015,
        ),
        TesterProfile(
            name="tester-4-mobile-4g",
            role="Musicista Hotspot 4G/LTE (Buffer 128, Jitter 16ms, Loss 2.0%)",
            buffer_frames=128,
            jitter_chunks=6,
            delay_ms=22,
            jitter_ms=16,
            loss=0.02,
        ),
    ]

    if preset == "churn":
        cycle_dur = max(3, total_seconds // 2)
        profiles[3].role += " + Reconnect Churn (2 cicli)"
        profiles[3].duration_s = cycle_dur
        profiles[3].reconnect_cycles = 2
    elif preset == "stress":
        profiles.extend(
            [
                TesterProfile(
                    name="tester-5-studio-64",
                    role="Seconda postazione Studio Low-Latency (Buffer 64)",
                    buffer_frames=64,
                    jitter_chunks=2,
                    delay_ms=4,
                    jitter_ms=1,
                    loss=0.0,
                ),
                TesterProfile(
                    name="tester-6-wifi-heavy",
                    role="Wi-Fi degradato (Buffer 256, Jitter 20ms, Loss 3.0%)",
                    buffer_frames=256,
                    jitter_chunks=6,
                    delay_ms=25,
                    jitter_ms=20,
                    loss=0.03,
                ),
            ]
        )

    for idx, profile in enumerate(profiles):
        if bind_ips:
            profile.bind_ip = bind_ips[idx % len(bind_ips)]
        if profile.duration_s is None:
            profile.duration_s = total_seconds

    return profiles


def find_free_udp_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


def binary_path(target_dir: Path, profile_dir: str, name: str) -> Path:
    exe = f"{name}.exe" if os.name == "nt" else name
    return target_dir / profile_dir / exe


def evaluate_gates(
    results: list[dict[str, Any]],
    expected_min_remote_peers: int,
) -> list[dict[str, Any]]:
    gates: list[dict[str, Any]] = []

    all_exited_zero = all(r["exit_code"] == 0 and r["telemetry_logs"] for r in results)
    gates.append(
        {
            "gate": "1. Avvio & Completamento Sessioni",
            "status": "PASS" if all_exited_zero else "FAIL",
            "details": "Tutti i processi client hanno completato la sessione con exit code 0 e generato i log.",
        }
    )

    all_summaries_ok = all(
        log["has_summary"] and log["dropped_samples"] == 0
        for r in results
        for log in r["telemetry_logs"]
    )
    gates.append(
        {
            "gate": "2. Integrità Logger Telemetria (JAM-20)",
            "status": "PASS" if all_summaries_ok else "FAIL",
            "details": "Summary finale presente e telemetry_dropped_samples=0 su tutti i file.",
        }
    )

    max_peers_seen = [
        (
            r["profile"]["name"],
            max(
                (
                    (log["summary"].get("remote_peers", {}).get("max_val") or 0.0)
                    for log in r["telemetry_logs"]
                ),
                default=0.0,
            ),
        )
        for r in results
    ]
    peers_ok = all(seen >= expected_min_remote_peers for _, seen in max_peers_seen)
    gates.append(
        {
            "gate": "3. Handshake UDP & Discovery Stanza",
            "status": "PASS" if peers_ok else "FAIL",
            "details": ", ".join(f"{name}: max { int(seen) } peer remoti" for name, seen in max_peers_seen),
        }
    )

    rx_drops = sum(
        int((log["summary"].get("rx_push_dropped", {}).get("max_val") or 0.0))
        for r in results
        for log in r["telemetry_logs"]
    )
    callbacks_ok = all(
        (log["summary"].get("output_callbacks", {}).get("max_val") or 0.0) > 0
        for r in results
        for log in r["telemetry_logs"]
    )
    gates.append(
        {
            "gate": "4. Continuità Motore Audio & Code SPSC (JAM-24)",
            "status": "PASS" if (rx_drops == 0 and callbacks_ok) else "FAIL",
            "details": f"rx_push_dropped totali={rx_drops}, callback output attive={callbacks_ok}.",
        }
    )

    stable_clean = True
    stable_notes = []
    for r in results:
        if r["profile"]["loss"] == 0.0 and r["profile"]["jitter_ms"] <= 2:
            for log in r["telemetry_logs"]:
                uf = int(log["summary"].get("output_underflows", {}).get("max_val") or 0.0)
                far = int(log["summary"].get("late_dropped_far", {}).get("max_val") or 0.0)
                stable_notes.append(f"{r['profile']['name']}: underflows={uf}, late_far={far}")
                if uf > 0 or far > 0:
                    stable_clean = False
    gates.append(
        {
            "gate": "5. Assenza Glitch su Peer Stabili (JAM-30)",
            "status": "PASS" if stable_clean else "WARN",
            "details": "; ".join(stable_notes) if stable_notes else "Nessun peer stabile configurato.",
        }
    )

    return gates


def format_markdown_report(
    server_addr: str,
    preset: str,
    elapsed_s: float,
    results: list[dict[str, Any]],
    gates: list[dict[str, Any]],
) -> str:
    lines = [
        "# Virtual Jam — Report Beta-Testing Autonomo Pre-Release",
        "",
        f"- **Server UDP**: `{server_addr}`",
        f"- **Profilo Suite**: `{preset}` ({len(results)} agenti beta-tester)",
        f"- **Durata Totale**: `{elapsed_s:.2f} s`",
        "",
        "## 1. Esito Quality Gates Pre-Release",
        "",
        "| Gate | Esito | Dettagli |",
        "| --- | --- | --- |",
    ]
    for g in gates:
        lines.append(f"| {g['gate']} | **{g['status']}** | {g['details']} |")

    lines.extend(
        [
            "",
            "## 2. Riepilogo Telemetria per Agente Beta-Tester",
            "",
            "| Agente | Buffer | Rete Simulata (Delay / Jitter / Loss) | RTT avg/p95 (ms) | Jitter p95 (ms) | Loss max (%) | Buffer p95 (ms) | E2E One-Way avg/p95 (ms) | Headroom avg (ms) | PLC max | Underflows | Gap Worker p95 (ms) |",
            "| --- | ---: | --- | --- | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: |",
        ]
    )

    def fmt(val: float | None, digits: int = 2) -> str:
        return f"{val:.{digits}f}" if val is not None else "N/A"

    for r in results:
        p = r["profile"]
        net_desc = f"{p['delay_ms']}ms / {p['jitter_ms']}ms / {p['loss']*100:.1f}% (c={p['jitter_chunks']})"
        if p.get("bind_ip"):
            net_desc += f" @ `{p['bind_ip']}`"
        for idx, log in enumerate(r["telemetry_logs"]):
            s = log["summary"]
            name_label = p["name"] if len(r["telemetry_logs"]) == 1 else f"{p['name']} (#{idx+1})"
            rtt = s.get("rtt_ms", {})
            jit = s.get("jitter_ms", {})
            loss = s.get("loss_percent", {})
            buf = s.get("buffer_ms", {})
            e2e = s.get("total_one_way_ms", {})
            head = s.get("headroom_ms", {})
            plc = s.get("plc_frames", {})
            uf = s.get("output_underflows", {})
            gap = s.get("worker_max_gap_ms", {})

            lines.append(
                f"| `{name_label}` | {p['buffer_frames']} | {net_desc} | "
                f"{fmt(rtt.get('avg_val'))} / {fmt(rtt.get('p95_val'))} | "
                f"{fmt(jit.get('p95_val'))} | "
                f"{fmt(loss.get('max_val'))} | "
                f"{fmt(buf.get('p95_val'))} | "
                f"{fmt(e2e.get('avg_val'))} / {fmt(e2e.get('p95_val'))} | "
                f"{fmt(head.get('avg_val'))} | "
                f"{int(plc.get('max_val') or 0)} | "
                f"{int(uf.get('max_val') or 0)} | "
                f"{fmt(gap.get('p95_val'))} |"
            )

    lines.extend(["", "## 3. File Log Generati", ""])
    for r in results:
        for log in r["telemetry_logs"]:
            lines.append(f"- `{r['profile']['name']}`: `{log['file']}` ({log['row_count']} campioni)")

    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description="Virtual Jam Autonomous Beta-Tester Runner")
    parser.add_argument("--server", help="Remote jam-server IP:PORT (if omitted, starts local jam-server)")
    parser.add_argument("--seconds", type=int, default=8, help="Session duration in seconds (default: 8)")
    parser.add_argument(
        "--preset",
        choices=["standard", "churn", "stress"],
        default="standard",
        help="Beta-tester cohort preset",
    )
    parser.add_argument("--bind-ips", help="Comma-separated local IPs to cycle across testers via --bind-ip")
    parser.add_argument("--output-dir", help="Directory for logs and report (default: logs/beta-<timestamp>)")
    parser.add_argument("--target-dir", help="Cargo target directory (default: target or CARGO_TARGET_DIR)")
    parser.add_argument("--debug", action="store_true", help="Use debug build instead of --release")
    parser.add_argument("--no-build", action="store_true", help="Skip cargo build step")
    args = parser.parse_args()

    workspace = Path(__file__).resolve().parent.parent
    target_dir = Path(
        args.target_dir
        or os.environ.get("CARGO_TARGET_DIR")
        or (workspace / "target")
    ).resolve()
    profile_dir = "debug" if args.debug else "release"

    if not args.no_build:
        build_cmd = [
            "cargo",
            "build",
            "--workspace",
            "--features",
            "jam-client/telemetry-log",
            "--target-dir",
            str(target_dir),
        ]
        if not args.debug:
            build_cmd.append("--release")
        print(f"[beta-testers] Compiling workspace: {' '.join(build_cmd)}", flush=True)
        subprocess.run(build_cmd, cwd=workspace, check=True)

    client_bin = binary_path(target_dir, profile_dir, "jam-client")
    server_bin = binary_path(target_dir, profile_dir, "jam-server")
    if not client_bin.exists():
        raise FileNotFoundError(f"jam-client binary not found at {client_bin}")

    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    out_dir = Path(args.output_dir or (workspace / "logs" / f"beta-{stamp}")).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)

    bind_ips = [ip.strip() for ip in args.bind_ips.split(",") if ip.strip()] if args.bind_ips else []
    profiles = build_profiles(args.preset, args.seconds, bind_ips)

    local_server_proc: subprocess.Popen[bytes] | None = None
    server_addr = args.server
    if not server_addr:
        if not server_bin.exists():
            raise FileNotFoundError(f"jam-server binary not found at {server_bin}")
        port = find_free_udp_port()
        server_addr = f"127.0.0.1:{port}"
        server_stdout = (out_dir / "server-stdout.txt").open("wb")
        server_stderr = (out_dir / "server-stderr.txt").open("wb")
        local_server_proc = subprocess.Popen(
            [str(server_bin), server_addr],
            cwd=workspace,
            stdout=server_stdout,
            stderr=server_stderr,
        )
        time.sleep(0.3)

    print(f"[beta-testers] Running {len(profiles)} autonomous beta-testers against {server_addr} -> {out_dir}", flush=True)
    start_time = time.monotonic()

    active_runs: list[dict[str, Any]] = []
    try:
        max_cycles = max(p.reconnect_cycles for p in profiles)
        for cycle in range(max_cycles):
            running_procs: list[tuple[dict[str, Any], subprocess.Popen[bytes], Any, Any]] = []
            for p in profiles:
                if cycle == 0:
                    tester_dir = out_dir / p.name
                    tester_dir.mkdir(parents=True, exist_ok=True)
                    active_runs.append(
                        {
                            "profile": dataclasses.asdict(p),
                            "dir": tester_dir,
                            "exit_code": 0,
                            "telemetry_logs": [],
                        }
                    )
                if cycle >= p.reconnect_cycles:
                    continue
                if cycle > 0 and p.reconnect_cycles == 1:
                    continue

                entry = next(r for r in active_runs if r["profile"]["name"] == p.name)
                tester_dir = entry["dir"]
                cmd = [
                    str(client_bin),
                    "--server",
                    server_addr,
                    "--nickname",
                    p.name[:32],
                    "--synthetic",
                    "--buffer-frames",
                    str(p.buffer_frames),
                    "--jitter-chunks",
                    str(p.jitter_chunks),
                    "--simulate-delay-ms",
                    str(p.delay_ms),
                    "--simulate-jitter-ms",
                    str(p.jitter_ms),
                    "--simulate-loss",
                    str(p.loss),
                    "--seconds",
                    str(p.duration_s),
                    "--telemetry-dir",
                    str(tester_dir),
                ]
                if p.metronome_bpm is not None:
                    cmd.extend(["--metronome-bpm", str(p.metronome_bpm)])
                if p.bind_ip is not None:
                    cmd.extend(["--bind-ip", p.bind_ip])

                stdout_f = (tester_dir / f"stdout-cycle{cycle+1}.txt").open("wb")
                stderr_f = (tester_dir / f"stderr-cycle{cycle+1}.txt").open("wb")
                proc = subprocess.Popen(cmd, cwd=workspace, stdout=stdout_f, stderr=stderr_f)
                running_procs.append((entry, proc, stdout_f, stderr_f))

            for entry, proc, stdout_f, stderr_f in running_procs:
                code = proc.wait()
                stdout_f.close()
                stderr_f.close()
                if code != 0:
                    entry["exit_code"] = code
    finally:
        if local_server_proc is not None:
            local_server_proc.terminate()
            try:
                local_server_proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                local_server_proc.kill()

    elapsed_s = time.monotonic() - start_time

    for entry in active_runs:
        tester_dir = entry["dir"]
        txt_files = sorted(tester_dir.glob("telemetry-*.txt"))
        entry["telemetry_logs"] = [parse_telemetry_file(f) for f in txt_files]
        entry["dir"] = str(tester_dir)

    expected_peers = max(1, len(profiles) - 1)
    gates = evaluate_gates(active_runs, expected_peers)
    report_md = format_markdown_report(server_addr, args.preset, elapsed_s, active_runs, gates)

    report_path = out_dir / "report.md"
    summary_path = out_dir / "summary.json"
    report_path.write_text(report_md, encoding="utf-8")
    summary_path.write_text(
        json.dumps(
            {
                "server": server_addr,
                "preset": args.preset,
                "elapsed_s": elapsed_s,
                "gates": gates,
                "testers": active_runs,
            },
            indent=2,
        ),
        encoding="utf-8",
    )

    print(report_md, flush=True)
    print(f"[beta-testers] Report written to {report_path}", flush=True)
    print(f"[beta-testers] JSON summary written to {summary_path}", flush=True)

    return 0 if all(g["status"] != "FAIL" for g in gates) else 1


if __name__ == "__main__":
    sys.exit(main())
