---
name: beta-testing
description: >-
  Esegue sessioni di beta-testing multi-agente e multi-processo sulle pre-release di
  Virtual Jam Hub, simulando profili di rete eterogenei (Fibra, VDSL, Wi-Fi, 4G/Hotspot),
  buffer hardware differenti (64, 128, 256 frame), routing multi-interfaccia (--bind-ip)
  e raccogliendo/validando i file telemetry-*.txt a 27 metriche.
---

# Beta-Testing Autonomo Pre-Release — Virtual Jam Hub

Usa questa skill per validare una pre-release (`1.0.x-beta`) quando non sono disponibili beta-tester umani o per verificare regressioni su rete, jitter buffer adattivo, budget di latenza E2E e sincronizzazione del metronomo.

## 1. Come viene aggirato il limite della "stessa macchina / stessa connessione"

1. **Processi UDP indipendenti (`0.0.0.0:0` o `--bind-ip IP`)**:
   Ogni agente lancia un processo `jam-client` distinto con socket UDP effimero proprio; `jam-server` li registra come peer separati nella `SessionTable`.
2. **Routing su interfacce fisiche diverse (`--bind-ip`)**:
   Verso un server remoto (VPS), gli agenti sulla stessa macchina possono uscire su reti fisiche diverse passando `--bind-ip <IP_LOCALE>` (es. Ethernet/Fibra + Wi-Fi/Hotspot 4G USB + VPN WireGuard).
3. **Profili di rete asimmetrici per-peer (`--simulate-delay-ms`, `--simulate-jitter-ms`, `--simulate-loss`, `--jitter-chunks`)**:
   Ogni processo client applica un ritardo base di transito (che sposta sia l'arrivo dei pacchetti audio sia l'RTT misurato dai Ping/Pong, attivando `LatencyBudget` JAM-31 e la compensazione di fase Tempo Leader JAM-34 anche in localhost), oltre a jitter uniforme e perdita pacchetti.
4. **Clock Audio Sintetico Headless (`--synthetic --buffer-frames 64|128|256`)**:
   Senza occupare driver ASIO esclusivi o schede fisiche, ogni processo esegue la cadenza esatta delle callback hardware (64, 128 o 256 frame a 48 kHz), sintetizza un tono continuo a 400 Hz, esegue il `Mixer::playback` reale (incluso soft-clipping, rampe anti-underflow JAM-30 e sintetizzatore metronomo JAM-33) e scrive il log completo a 27 metriche tramite `AsyncSessionLog` (`--telemetry-dir`).

## 2. Modalità di Esecuzione

### A. Suite Multi-Processo Automatica (`scripts/beta-testers.ps1` su Windows / `scripts/beta_testers.py` su Linux & macOS)
Avvia un `jam-server` locale (o usa `-Server IP:PORT` / `--server IP:PORT` per una VPS remota), lancia la squadra di beta-tester in parallelo e genera `report.md` e `summary.json`:

```powershell
# Windows (PowerShell nativo, zero dipendenze esterne):
# Sessione standard (4 beta-tester: Fibra Leader 64f, VDSL 128f, Wi-Fi 256f, 4G 128f)
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/beta-testers.ps1 -Preset standard -Seconds 10

# Sessione con churn (disconnessione e riconnessione a metà sessione)
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/beta-testers.ps1 -Preset churn -Seconds 12

# Sessione stress a 6 musicisti
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/beta-testers.ps1 -Preset stress -Seconds 15

# Test verso VPS remota uscendo su due interfacce di rete diverse (es. Fibra + Hotspot 4G)
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/beta-testers.ps1 -Server 185.198.245.40:7777 -BindIps "192.168.1.50,172.20.10.2" -Seconds 30
```

### B. Benchmark Veloce Multi-Profilo in Singolo Processo (`net_bench --heterogeneous`)
Per un benchmark rapido in-process con profili di rete differenziati per peer:

```powershell
cargo run --release -p jam-client --example net_bench -- 127.0.0.1:7777 --peers 4 --seconds 10 --heterogeneous
```

### C. Orchestrazione Multi-Agente AI (`run_workflow` con `scripts/beta_agent_workflow.py`)
Per far analizzare i log di ogni musicista a un subagente dedicato in parallelo:
- Invoca il tool `run_workflow` con `ScriptPath` impostato a `scripts/beta_agent_workflow.py` e `Args` opzionali (`{"preset": "standard", "seconds": 8}`).
