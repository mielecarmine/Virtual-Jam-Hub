See `.rust-skills/AGENTS.md` for Rust development guidelines.

## Flusso obbligatorio per i ticket JAM

Quando l'utente chiede di partire con un nuovo ticket, seguire sempre questo ordine:

1. Leggere il ticket completo nella JAM board collegata alla repository.
2. Spostare il ticket in `In progress`.
3. Aprire un branch dedicato da `develop` aggiornato (prefisso `codex/`).
4. Implementare il ticket ed eseguire i test e tutti i gate di verifica del progetto.
5. Presentare il risultato e attendere l'approvazione esplicita dell'utente prima del merge.
6. Dopo l'approvazione, effettuare il merge manuale su `develop`.
7. Spostare il ticket in `Done` solo dopo il merge.

Se un permesso impedisce un passaggio, segnalarlo senza dichiararlo completato.

## Virtual Jam — Regole Architetturali & Real-Time Guardrails

### Struttura Monorepo (Cargo Workspace)

- `crates/jam-protocol`: libreria condivisa (costanti, struct binarie `#[repr(C)]`, serializzazione zero-copy). Nessuna dipendenza da OS/audio/rete.
- `crates/jam-server`: demone SFU UDP headless per Linux (`1 -> N-1` packet reflector, `SessionTable` con watchdog 5s).
- `crates/jam-client`: client desktop (motore audio `cpal` + `rtrb`, jitter buffer per singolo peer, codec `audiopus`, GUI `eframe`/`egui`).

### Vincoli Inviolabili (Non negoziabili)

1. **Audio Thread Real-Time (`cpal` callbacks):**
   - VIETATO allocare memoria sull'heap (`Box`, `Vec`, `String`, `format!`, `.to_owned()`).
   - VIETATO acquisire `Mutex`, `RwLock`, canali MPSC standard o fare syscall/I/O bloccanti.
   - Comunicazione tra Audio Thread, Network Thread e UI **solo** tramite ring buffer lock-free SPSC (`rtrb`) e atomici (`AtomicU32` con `f32::to_bits()`, `AtomicBool`).
2. **Networking & Protocollo:**
   - **Solo UDP**, mai TCP (evitare _Head-of-Line Blocking_).
   - Nessun formato testuale (no JSON/MessagePack) sul wire: usare layout binari fissi con endianness esplicita (`to_le_bytes`/`from_le_bytes`) e payload totali sempre `< 1400` byte (sotto MTU).
   - Jitter buffer e decoder Opus devono essere istanziati **indipendentemente per ogni `peer_id`**, mai condivisi tra peer diversi.

### Workflow & Verifica Prima di Completare un Task

- Ogni modifica a `jam-protocol` deve includere unit test di serializzazione round-trip.
- Prima di chiudere qualsiasi modifica, eseguire sempre:
  - `cargo fmt --check`
  - `cargo clippy --workspace -- -D warnings`
  - `cargo test --workspace`
