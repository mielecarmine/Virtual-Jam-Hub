# Dynamic workflow for `run_workflow`: orchestrates autonomous Beta-Tester agents
# across a Virtual Jam pre-release session and cross-checks their telemetry logs.

import json
import os
from pathlib import Path
import subprocess
import sys

workspace = Path(WORKSPACES[0])
preset = args.get("preset", "standard") if isinstance(args, dict) else "standard"
seconds = int(args.get("seconds", 6)) if isinstance(args, dict) else 6
server = args.get("server") if isinstance(args, dict) else None
bind_ips = args.get("bind_ips") if isinstance(args, dict) else None

phase("1. Run Multi-Process Synthetic Jam Session")
if os.name == "nt":
    cmd = [
        "powershell",
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        str(workspace / "scripts" / "beta-testers.ps1"),
        "-Preset",
        str(preset),
        "-Seconds",
        str(seconds),
    ]
    if server:
        cmd.extend(["-Server", str(server)])
    if bind_ips:
        cmd.extend(["-BindIps", str(bind_ips)])
else:
    cmd = [
        sys.executable,
        str(workspace / "scripts" / "beta_testers.py"),
        "--preset",
        str(preset),
        "--seconds",
        str(seconds),
    ]
    if server:
        cmd.extend(["--server", str(server)])
    if bind_ips:
        cmd.extend(["--bind-ips", str(bind_ips)])

log(f"Launching beta-tester cohort: {' '.join(cmd)}")
proc = subprocess.run(cmd, cwd=workspace, capture_output=True, text=True)
if proc.returncode != 0 and "summary.json" not in proc.stdout:
    raise RuntimeError(f"Beta-tester runner failed:\n{proc.stdout}\n{proc.stderr}")

summary_line = next(
    (
        line
        for line in proc.stdout.splitlines()
        if "JSON summary written to " in line or "JSON summary scritto in " in line
    ),
    None,
)
if not summary_line:
    raise RuntimeError(f"Could not locate summary.json path in output:\n{proc.stdout}")

marker = "JSON summary written to " if "JSON summary written to " in summary_line else "JSON summary scritto in "
summary_path = Path(summary_line.split(marker, 1)[1].strip())
summary_data = json.loads(summary_path.read_text(encoding="utf-8"))
testers = summary_data["testers"]
log(f"Session completed. {len(testers)} tester profiles captured in {summary_path.parent}")

phase("2. Parallel Per-Agent Telemetry Audit")
audit_schema = {
    "type": "object",
    "properties": {
        "tester": {"type": "string"},
        "verdict": {"type": "string", "enum": ["PASS", "WARN", "FAIL"]},
        "e2e_latency_assessment": {"type": "string"},
        "jitter_and_plc_assessment": {"type": "string"},
        "anomalies": {"type": "array", "items": {"type": "string"}},
    },
    "required": [
        "tester",
        "verdict",
        "e2e_latency_assessment",
        "jitter_and_plc_assessment",
        "anomalies",
    ],
}

audits = await parallel(
    testers,
    lambda t: agent(
        prompt=(
            f"Sei l'agente beta-tester '{t['profile']['name']}' ({t['profile']['role']}).\n"
            f"Configurazione usata: {json.dumps(t['profile'])}\n"
            f"Riepilogo log telemetry-*.txt raccolti: {json.dumps(t['telemetry_logs'], indent=2)}\n\n"
            "Analizza i tuoi log di sessione come musicista beta-tester:\n"
            "1. Verifica che `telemetry_dropped_samples` sia 0 e `rx_push_dropped` sia 0.\n"
            "2. Valuta il budget di latenza E2E (`total_one_way_ms`, `headroom_ms`, `latency_class`) rispetto al tuo profilo.\n"
            "3. Valuta il comportamento del jitter buffer adattivo (`buffer_ms`), PLC (`plc_frames`), `late_dropped_far` e `output_underflows`.\n"
            "Restituisci il tuo verdetto strutturato tramite il tool `finish`."
        ),
        schema=audit_schema,
        role=f"Beta-Tester {t['profile']['name']}",
        type_name="research",
    ),
)

for a in audits:
    log(f"[{a['tester']}] Verdict={a['verdict']} | E2E: {a['e2e_latency_assessment']} | Jitter/PLC: {a['jitter_and_plc_assessment']}")

print(
    json.dumps(
        {
            "report_md": str(summary_path.parent / "report.md"),
            "summary_json": str(summary_path),
            "gates": summary_data["gates"],
            "agent_audits": audits,
        },
        indent=2,
    )
)
