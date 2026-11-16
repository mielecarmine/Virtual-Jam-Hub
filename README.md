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

### Header audio e flussi

`AudioHeader` mantiene 12 byte e allineamento C a 4 byte. Il wire usa little-endian:

| Offset | Campo | Byte |
| --- | --- | --- |
| 0 | magic | 2 |
| 2 | packet_type | 1 |
| 3 | peer_id | 1 |
| 4 | sequence_number | 4 |
| 8 | timestamp_samples | 2 |
| 10 | stream_id | 1 |
| 11 | reserved | 1 |

`AudioHeader::new(peer_id, stream_id, sequence_number, timestamp_samples)` inizializza
`reserved` a zero. `STREAM_PRIMARY = 0` identifica lo strumento principale e
`STREAM_TALKBACK = 1` la voce; gli altri ID di flusso restano disponibili.
Sequenza e timestamp avanzano con `wrapping_add`, rispettivamente modulo 2^32 e 2^16.
La serializzazione opera direttamente su slice, senza allocazioni, anche su input
non allineato. Il server inoltra tutti i flussi senza modificarli.

Questo layout sostituisce il precedente timestamp a 32 bit: client e server devono
usare la stessa versione del protocollo, perché il vecchio formato non è compatibile.

### 6. Avvio del server UDP

Per installare il servizio su Linux con systemd e verificare handshake/Pong
dalla rete domestica, seguire [deploy/README.md](deploy/README.md).

```bash
cargo run -p jam-server --release
```

Il server ascolta su `0.0.0.0:7777` tramite UDP. Il timeout di ricezione di 100 ms
consente al watchdog di controllare le sessioni anche in assenza di traffico.
Ogni stanza accetta al massimo 8 endpoint, assegnando il primo ID libero tra 1 e 8.
Un `JoinRequest` ripetuto dallo stesso endpoint conserva l'ID e aggiorna il nickname.
All'ingresso, all'uscita con `Leave` e alla scadenza di una sessione, tutti i peer
rimanenti ricevono una `PeerListUpdate` ordinata per ID.

I client devono inviare pacchetti audio validi con il proprio ID oppure `Ping`
per mantenere attiva la sessione; i `Ping` ricevono un `Pong`. Dopo oltre 5 secondi
di inattività il server rimuove il peer e libera il suo ID. I datagrammi malformati,
di almeno 1400 byte o con ID non corrispondente all'endpoint non rinnovano la sessione.
Quando la stanza è piena, i nuovi ingressi vengono ignorati: il protocollo attuale
non definisce un messaggio di rifiuto.

Ogni pacchetto audio valido viene inoltrato, senza modificare header o payload,
a tutti gli altri peer attivi. Il mittente non riceve il proprio audio.
L'ID nell'header deve coincidere con quello assegnato all'endpoint UDP: i pacchetti
con ID contraffatto vengono scartati senza aggiornare il watchdog.

I test del server usano socket UDP locali su porte effimere:

```bash
cargo test -p jam-server
```

La suite include lo stress test `tests/load_test.rs`: cinque thread bot completano
il join su un server locale con porta effimera e inviano ciascuno 4.000 datagrammi
da 120 byte, uno ogni 2,5 ms, per 10 secondi. Il carico complessivo è di 2.000
pacchetti/s in ingresso e 8.000 in uscita. Ogni bot deve ricevere almeno il 99,9%
dei 16.000 pacchetti degli altri quattro, senza duplicati, riordino per mittente
o eco locale. Il test controlla anche header e payload e lascia 300 ms finali
per drenare i datagrammi in transito. Non è ignorato nella suite ordinaria.
Per vedere i conteggi per bot:

```bash
cargo test -p jam-server --test load_test -- --nocapture
```

Il gate di latenza si esegue separatamente in release su una macchina scarica:
verifica 500 frame con 5 peer attivi e richiede un massimo inferiore a 200 µs.
La misura con `Instant` parte subito dopo `recv_from` e termina dopo l'ultimo
tentativo di `send_to`, includendo validazione e watchdog; esclude l'attesa di rete.
`Server::last_reflection_duration()` espone questa durata per l'ultimo poll audio.

```bash
cargo test --release -p jam-server --test udp_reflector five_peer_forwarding_stays_below_200_microseconds -- --ignored --nocapture
```

### 7. Loopback audio hardware locale — JAM-8

Il client CLI collega l'ingresso audio all'uscita locale senza server o rete:

```text
cpal input -> mic_to_net -> thread local-loopback -> net_to_speaker -> cpal output
```

Servono dispositivi di ingresso e uscita che supportino
48.000 Hz e un buffer fisso di **128 frame per canale** (circa 2,67 ms).
La dimensione del buffer richiesta non impone la lunghezza di ogni callback:
ALSA può consegnare periodi più corti o accorparli. Il client elabora tutti
i frame ricevuti, mantenendo la richiesta `BufferSize::Fixed(128)`.
Su Linux servono anche `pkg-config` e gli header ALSA (`libasound2-dev` su Debian/Ubuntu).
Senza opzioni il client usa backend e dispositivi predefiniti del sistema.
Il client preferisce il formato `f32`, poi la configurazione con meno canali: acquisisce il primo
canale di ingresso come segnale mono e lo riproduce su tutti i canali di uscita.
Quando il driver espone un altro formato, il client converte i campioni in `f32`
all'ingresso e nel formato hardware all'uscita, senza allocare memoria nei callback.
Supporta i dieci formati di campione di `cpal` 0.15 (`f32`, `f64`, interi signed/unsigned
da 8, 16, 32 e 64 bit). La frequenza rimane 48 kHz, senza ricampionamento.
Backend, dispositivi e formati selezionati vengono stampati all'avvio.

```bash
# Prova di accettazione: cinque minuti effettivi dopo l'avvio della riproduzione.
cargo run --locked --release -p jam-client

# Prova breve o durata personalizzata; Ctrl+C interrompe anticipatamente.
cargo run --locked --release -p jam-client -- --seconds 10
cargo run --locked -p jam-client -- --help
```

Per scegliere esplicitamente driver e dispositivi:

```bash
# Linux: elenca i nomi ALSA disponibili (i dispositivi occupati possono essere assenti).
cargo run --locked --release -p jam-client -- --host alsa --list-devices

# Linux: percorso ALSA/PipeWire condiviso.
cargo run --locked --release -p jam-client -- --host alsa --input pipewire --output pipewire

# Linux: hardware diretto; sostituire il nome con quello riportato dall'elenco.
cargo run --locked --release -p jam-client -- --host alsa --input 'hw:CARD=USB,DEV=0' --output 'hw:CARD=USB,DEV=0'

# Windows: elenco driver ASIO e scelta del driver Focusrite.
cargo run --locked --release -p jam-client -- --host asio --list-devices
cargo run --locked --release -p jam-client -- --host asio --input 'Focusrite USB ASIO' --output 'Focusrite USB ASIO'

# macOS: CoreAudio; Windows offre anche --host wasapi.
cargo run --locked --release -p jam-client -- --host coreaudio --list-devices
```

I nomi dei backend non distinguono maiuscole/minuscole; i nomi dei dispositivi
devono coincidere esattamente con l'elenco. Una scelta non disponibile provoca
un errore esplicito, senza ripiego su un altro dispositivo o backend.
ASIO è abilitato nelle build Windows: per compilarlo occorrono gli strumenti
C++ MSVC, LLVM/libclang e l'ASIO SDK richiesti da `cpal` (variabili `LIBCLANG_PATH`
e `CPAL_ASIO_DIR`, se necessarie). Per eseguirlo serve il driver del produttore.
ALSA diretto richiede che l'interfaccia non sia occupata da PipeWire o altre applicazioni;
il client non arresta servizi audio automaticamente. PipeWire resta accessibile
tramite i dispositivi ALSA `default`/`pipewire`; non è un backend nativo separato di `cpal`.
La compatibilità del driver con 48 kHz e 128 frame è verificata prima dell'avvio.
La verifica hardware su Windows/ASIO e macOS/CoreAudio va eseguita sui rispettivi sistemi.

Usare cuffie per evitare feedback acustico. I due ring buffer `rtrb::RingBuffer<f32>`
contengono ciascuno 4096 campioni mono; tutta la memoria viene allocata prima
dell'avvio degli stream. I callback dati eseguono solo accessi a slice, operazioni
SPSC e contatori atomici; anche i callback di errore aggiornano solo atomici.
Stampe, attese e gestione della durata sono esterni ai callback.

All'avvio l'uscita resta silenziosa fino alla disponibilità di 256 campioni
(due blocchi, circa 5,33 ms) per assorbire lo sfasamento iniziale tra i callback.
Il worker trasferisce i campioni disponibili e attende 250 µs tra le iterazioni.
La capacità di 4096 campioni è un limite, non una coda mantenuta intenzionalmente piena.
La latenza fisica totale include driver, dispositivi e scheduling: va misurata
con un segnale e una registrazione esterna; il programma non la misura.
Interfacce con clock indipendenti possono accumulare deriva: questo loopback
non implementa ricampionamento o compensazione del clock.

L'overflow scarta i nuovi campioni di ingresso; l'underflow riempie con silenzio
i campioni mancanti. Entrambi vengono contati. Il client interrompe la prova
se rileva campioni persi, errori `cpal` o buffer che contengono frame incompleti,
e termina con codice non zero. Un dispositivo o formato non disponibile causa
un errore esplicito; un ingresso che non produce audio causa un timeout di 5 secondi.
Il riepilogo finale riporta callback, campioni persi e contatori di errore.

Per accettare la prova hardware, lasciare terminare tutti i 300 secondi,
verificare che i contatori di errore siano zero e confermare all'ascolto
l'assenza di click/interruzioni. Un arresto anticipato non completa la prova.
I test automatici verificano configurazione, ordine dei campioni, canali,
wrap-around, overflow, underflow e zero allocazioni nei percorsi audio;
non sostituiscono l'ascolto su hardware reale.

### 8. Audio PCM via server UDP — JAM-9

Per collegare due musicisti, avviare `jam-server` e usare lo stesso indirizzo
**IP:porta** su entrambi i client (IPv6: `[IP]:porta`). Per esempio, su una LAN:

```bash
# Server
cargo run --locked --release -p jam-server

# PC A (sostituire l'IP con quello del server)
cargo run --locked --release -p jam-client -- --server 192.168.1.10:7777 --nickname chitarra --seconds 300

# PC B, con il proprio dispositivo audio
cargo run --locked --release -p jam-client -- --server 192.168.1.10:7777 --nickname basso --seconds 300
```

Le opzioni `--host`, `--input` e `--output` restano disponibili per scegliere
schede diverse. Senza `--server` continua a funzionare il loopback locale JAM-8.
In modalità rete si ascolta solo l'audio remoto; il monitoraggio locale hardware,
se desiderato, va attivato sulla propria interfaccia. Usare cuffie.

Il thread `udp-network` possiede un socket UDP connesso non bloccante: invia
`JoinRequest` ogni 500 ms fino all'assegnazione dell'ID e `Ping` ogni secondo dopo
il join. Un server silenzioso dopo il join provoca un errore dopo 5 secondi.
Ctrl+C o la scadenza della durata arrestano gli stream, terminano il worker e
inviano `Leave` al meglio delle possibilità di UDP. La durata in modalità rete
parte dall'avvio degli stream, anche se il secondo musicista non è ancora presente.
L'attesa del join resta interrompibile e viene mostrato l'ID assegnato.

Ogni datagramma audio contiene l'header condiviso da 12 byte seguito da **128
campioni mono `f32` little-endian**: 524 byte totali, circa 1,57 Mbit/s per flusso
incluso l'header JAM (esclusi UDP/IP). Sequenza e timestamp avanzano con wrap-around.
Non c'è un indicatore del codec sul wire: entrambi i client devono usare questa
stessa modalità PCM. Il server SFU inoltra il payload senza interpretarlo.
Pacchetti con lunghezza errata, header non valido, stream diverso dal primario,
eco locale o campioni non finiti/fuori da `[-1, 1]` vengono scartati; un buffer di
uscita pieno scarta l'intero frame ricevuto.

Questa tappa verifica **due musicisti**: i pacchetti remoti passano al buffer
speaker nell'ordine di arrivo. Riordino/jitter buffer indipendenti per peer,
mixer multicanale e Opus sono previsti nei ticket JAM-10, JAM-11 e JAM-12.
Non usare questa modalità per valutare il mix di tre o più partecipanti.
Gli underflow di rete producono silenzio e vengono contati senza interrompere
la sessione; i problemi hardware `cpal` restano fatali. Gli overflow d'ingresso
vengono contati e scartati. Il loopback mantiene i suoi criteri rigorosi JAM-8.

I test automatici verificano retry, heartbeat, framing PCM, wrap-around,
validazione dei pacchetti, isolamento dell'endpoint UDP, timeout e `Leave`,
oltre allo scambio bidirezionale senza eco fra due worker e un vero `jam-server`
su localhost. Per completare l'accettazione hardware, avviare i due client su
PC o schede diverse e verificare all'ascolto che entrambi ricevano lo strumento
dell'altro in tempo reale. Questo ascolto non è sostituito dai test automatici.
