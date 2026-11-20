# Contribuire a Virtual Jam

Grazie per l'interesse nel contribuire a **Virtual Jam**! Questo documento descrive
le linee guida tecniche, i vincoli real-time inviolabili e il flusso di lavoro per
proporre modifiche tramite Pull Request.

---

## 1. Struttura del Monorepo (Cargo Workspace)

Il progetto è un workspace Rust (`edition = "2024"`, MSRV `1.85`) composto da quattro crate:

- `crates/jam-protocol`: libreria condivisa con costanti, header/messaggi binari `#[repr(C)]` e serializzazione zero-copy. **Nessuna dipendenza da OS, audio o rete.**
- `crates/jam-server`: demone SFU UDP headless (`1 -> N-1` packet reflector, `SessionTable` con watchdog 5 s, signaling P2P e sincronizzazione clock/metronomo).
- `crates/jam-client`: client desktop (motore audio `cpal` + `rtrb`, jitter buffer adattivo per singolo peer, codec `opus`, mixer DSP, motore P2P ibrido e GUI `eframe`/`egui`).
- `crates/jam-collector`: servizio HTTP opzionale per la raccolta centralizzata di log operativi e report anonimi di beta-testing.

---

## 2. Vincoli Architetturali e Real-Time (Non Negoziabili)

Ogni contributo deve rispettare rigorosamente le seguenti regole per preservare la latenza end-to-end sotto i 20–25 ms e l'assenza di glitch audio:

1. **Audio Thread Real-Time (`cpal` callbacks):**
   - **VIETATO** allocare memoria sull'heap (`Box`, `Vec`, `String`, `format!`, `.to_owned()`).
   - **VIETATO** acquisire `Mutex`, `RwLock`, canali MPSC standard o eseguire syscall / I/O bloccanti (rete, disco, logging, sleep).
   - La comunicazione tra Audio Thread, Network Thread e UI Thread deve avvenire **esclusivamente** tramite ring buffer lock-free SPSC (`rtrb`) e atomici (`AtomicU32` con `f32::to_bits()`, `AtomicBool`, `AtomicU64`).
2. **Networking & Protocollo:**
   - **Solo UDP** per streaming audio e controllo real-time (mai TCP, per evitare *Head-of-Line Blocking*).
   - Nessun formato testuale (JSON, MessagePack, ecc.) sul wire audio/controllo: usare layout binari fissi con endianness esplicita (`to_le_bytes` / `from_le_bytes`) e datagrammi sempre `< 1400` byte (sotto MTU).
   - Ogni modifica a `jam-protocol` deve includere **unit test di serializzazione round-trip** e validazione dei casi limite.
3. **Isolamento per Peer:**
   - Jitter buffer (`PeerJitterBuffer`) e decoder Opus devono essere istanziati **indipendentemente per ogni `peer_id`**, mai condivisi tra peer diversi.
4. **Qualità Rust & Sicurezza:**
   - Evitare `unsafe`; se strettamente necessario, documentare ogni blocco con un commento `// SAFETY:` esaustivo.
   - Evitare `unwrap()` / `expect()` nel codice di libreria e nei percorsi runtime non-test; propagare gli errori in modo esplicito (`Result` / `?`).
   - Rispettare i lint del workspace (`clippy::all` e `clippy::pedantic` come errori in CI).

---

## 3. Prerequisiti di Compilazione

- **Toolchain Rust:** `rustc >= 1.85` (con `rustfmt` e `clippy`).
- **Opus (`opusic-sys`):** CMake e un compilatore C/C++.
- **Linux:** `pkg-config`, `libasound2-dev`, `libxkbcommon-dev`, `libwayland-dev`, `libx11-dev`, `libxcursor-dev`, `libxi-dev`, `libxrandr-dev`, `libgl1-mesa-dev`.
- **Windows:** strumenti C++ MSVC, LLVM/`libclang` e Steinberg ASIO SDK (`CPAL_ASIO_DIR`, `LIBCLANG_PATH`) per la build con backend ASIO.
- **macOS:** Xcode Command Line Tools (CoreAudio).

---

## 4. Flusso di Lavoro e Pull Request

1. **Apri o verifica una Issue:** prima di modifiche architetturali o nuove feature, apri una issue per discutere l'approccio.
2. **Crea un branch dedicato** a partire dal branch `develop` (o dal branch di pre-release attivo):
   ```bash
   git checkout develop
   git pull origin develop
   git checkout -b feat/breve-descrizione
   ```
3. **Implementa e aggiungi i test:**
   - Aggiungi unit test deterministici (e test di zero-allocazione se tocchi percorsi audio).
   - Se modifichi il protocollo wire o il comportamento utente, aggiorna `README.md`.
4. **Esegui tutti i gate di verifica locali prima della PR:**
   ```bash
   cargo fmt --check
   cargo clippy --workspace -- -D warnings
   cargo test --workspace
   cargo clippy --workspace --features jam-client/telemetry-log -- -D warnings
   cargo test --workspace --features jam-client/telemetry-log
   ```
   Se disponi di `cargo-deny`, verifica anche la conformità delle licenze delle dipendenze:
   ```bash
   cargo deny check licenses
   ```
5. **Apri la Pull Request:** descrivi chiaramente contesto, modifiche, test eseguiti ed eventuali verifiche hardware manuali richieste (es. ASIO/CoreAudio/ALSA).

---

## 5. Licenza dei Contributi

Virtual Jam è distribuito sotto licenza **GNU General Public License v3.0 o successiva (`GPL-3.0-or-later`)**, con l'eccezione aggiuntiva ai sensi della Sezione 7 della GPLv3 per il linking con lo Steinberg ASIO SDK sulle build Windows (vedi sezione **License** in [README.md](README.md)).

Inviando una Pull Request, accetti che il tuo contributo sia distribuito secondo i medesimi termini di licenza del progetto.
