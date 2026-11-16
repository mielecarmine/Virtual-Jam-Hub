# Virtual Jam — Sala Prove Virtuale Real-Time in Rust

## Sistema audio di rete a bassissima latenza progettato per consentire a una band (fino a 8 musicisti) di suonare insieme in tempo reale da remoto. L'obiettivo ingegneristico principale è mantenere la latenza end-to-end complessiva (Audio I/O + Codec + Rete) sotto la soglia psicoacustica dei 20–25 ms.

### 1. Architettura del Sistema

Il progetto è strutturato come un Monorepo (Cargo Workspace) composto da 3 crate che implementano un'architettura Client-Server centralizzata in modalità SFU (Selective Forwarding Unit) su protocollo UDP:
Plaintext

virtual-jam/
├── Cargo.toml # Workspace root (resolver = "2")
└── crates/
├── jam-protocol/ # Libreria condivisa: struct pacchetti binari, costanti, serializzazione zero-copy
├── jam-server/ # Binario headless (Linux systemd): Packet Reflector UDP (1 -> N-1) e SessionTable
└── jam-client/ # Binario desktop: motore Audio RT (cpal), Jitter Buffer, Mixer DSP, Opus e GUI (egui)

Flusso Dati

    Acquisizione (jam-client): Il callback audio cattura blocchi da 128 campioni a 48 kHz (~2.66 ms), li spinge in un ring buffer lock-free verso il thread di rete, li comprime con Opus (LowDelay) e li invia via UDP al server.

    Inoltro (jam-server): Il demone sul VPS valida l'header binario (12 byte) e re-instrada immediatamente il pacchetto grezzo a tutti gli altri N-1 peer connessi senza decodificare l'audio (elaborazione interna < 0.2 ms).

    Riproduzione (jam-client): Il client riceve i flussi separati per ogni peer_id, riordina i pacchetti tramite un Jitter Buffer adattivo indipendente per peer, decodifica Opus (con Packet Loss Concealment), applica gain/mute/solo e somma i canali con soft-clipping prima di inviarli alla scheda audio.

### 2. Regole Ingegneristiche Critiche (AI Agent Guardrails)

Ogni modifica al codice generata da sviluppatori o agenti AI deve rispettare tassativamente i seguenti vincoli:

    Zero-Allocation & Lock-Free nel Thread Audio (cpal):

        All'interno dei callback di input e output di cpal è severamente vietato: allocare memoria sull'heap (Box, Vec, String, format!), acquisire Mutex o RwLock, eseguire syscall o I/O di rete/disco.

        La comunicazione tra Audio Thread, Network Thread e UI Thread deve avvenire esclusivamente tramite ring buffer SPSC lock-free (rtrb) o tipi atomici (AtomicU32 per f32::to_bits(), AtomicBool).

    Solo UDP, Zero TCP:

        Non introdurre mai socket TCP per lo streaming o il controllo per evitare Head-of-Line Blocking. Perdite di pacchetti e riordino si gestiscono a livello applicativo tramite sequence_number e Opus PLC.

    Protocollo Binario Zero-Copy:

        Non usare JSON, MessagePack o formati testuali nei pacchetti di rete. Usare layout binari fissi (#[repr(C)] / bytemuck) con endianness esplicita (to_le_bytes / from_le_bytes) e dimensione pacchetto sempre inferiore a 1400 byte (sotto MTU).

    Isolamento per Peer:

        Non mescolare mai pacchetti provenienti da musicisti diversi nello stesso Jitter Buffer. Ogni peer_id ha latenza e jitter differenti e richiede una propria coda di riordino e una propria istanza del decoder Opus.

    Direct Monitoring Locale:

        Il client non riceve mai indietro il proprio flusso audio dal server per evitare eco ritardata.

### 3. Stack Tecnologico & Crate di Riferimento

Audio I/O Hardware: cpal (48 kHz, f32, buffer size fisso 64/128/256 campioni)

Concorrenza Real-Time: rtrb (ring buffer SPSC wait-free), std::sync::atomic

Networking: std::net::UdpSocket (non bloccante)

Codec Audio: audiopus (modalità Application::LowDelay, frame 2.5 ms / 5 ms)

Serializzazione: bytemuck (Pod, Zeroable)

Interfaccia Grafica: eframe / egui

### 4. Roadmap di Sviluppo (Epiche)

EPIC 1 — Fondazione & Protocollo (jam-protocol): Setup workspace, definizione header audio (12B), messaggi di controllo (JoinRequest, JoinAccepted, PeerListUpdate, Ping/Pong) e unit test round-trip.

EPIC 2 — Service SFU sul VPS (jam-server): Socket UDP non bloccante, SessionTable con watchdog timeout a 5s, inoltro 1 -> N-1, stress test locale con 5 bot e deploy su Linux via systemd.

EPIC 3 — Motore Audio & Rete (jam-client CLI): Loopback hardware cpal + rtrb, streaming PCM raw su UDP, Jitter Buffer per singolo peer con simulatore di packet loss/jitter, mixer RT con soft-clipping e integrazione Opus.

EPIC 4 — Interfaccia Grafica & UX (jam-client GUI): Setup dispositivi audio in egui, channel strip dinamiche (fader dB, Mute, Solo) e telemetria live (VU meter, Ping RTT, Jitter, Packet Loss).

### 5. Comandi Utili

Bash

#### Compilazione dell'intero workspace

cargo build --workspace

#### Compilazione ottimizzata per test di latenza e deploy VPS

cargo build --release --workspace

#### Esecuzione unit test e test di integrazione

cargo test --workspace

#### Controllo rigoroso qualità del codice e formattazione

cargo clippy --workspace -- -D warnings
cargo fmt --check
