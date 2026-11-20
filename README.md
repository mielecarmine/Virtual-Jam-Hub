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

EPIC 3 — Motore Audio & Rete (jam-client CLI): Loopback hardware cpal + rtrb, streaming Opus su UDP, Jitter Buffer per singolo peer con simulatore di packet loss/jitter, mixer RT con soft-clipping e integrazione Opus.

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
Nella modalità CLI, senza selezioni esplicite il client usa backend e dispositivi
predefiniti del sistema. L'avvio senza argomenti apre ora la GUI descritta nella sezione 12.
Il client preferisce il formato `f32`, poi la configurazione con meno canali: acquisisce il primo
canale di ingresso come segnale mono e lo riproduce su tutti i canali di uscita.
Quando il driver espone un altro formato, il client converte i campioni in `f32`
all'ingresso e nel formato hardware all'uscita, senza allocare memoria nei callback.
Supporta i dieci formati di campione di `cpal` 0.15 (`f32`, `f64`, interi signed/unsigned
da 8, 16, 32 e 64 bit). La frequenza rimane 48 kHz, senza ricampionamento.
Backend, dispositivi e formati selezionati vengono stampati all'avvio.

```bash
# Prova di accettazione: cinque minuti effettivi dopo l'avvio della riproduzione.
cargo run --locked --release -p jam-client -- --seconds 300

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

### 8. Audio via server UDP — JAM-9 e JAM-12

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
In modalità rete si ascoltano i peer e il proprio ingresso tramite monitoraggio
software diretto (JAM-18), anche nella CLI. Usare cuffie e disattivare il monitoraggio
hardware dell'interfaccia se si vuole evitare un doppio ascolto.

Il thread `udp-network` possiede un socket UDP connesso non bloccante: invia
`JoinRequest` ogni 500 ms fino all'assegnazione dell'ID e `Ping` ogni secondo dopo
il join. Un server silenzioso dopo il join provoca un errore dopo 5 secondi.
Ctrl+C o la scadenza della durata arrestano gli stream, terminano il worker e
inviano `Leave` al meglio delle possibilità di UDP. La durata in modalità rete
parte dall'avvio degli stream, anche se il secondo musicista non è ancora presente.
L'attesa del join resta interrompibile e viene mostrato l'ID assegnato.

Ogni datagramma audio contiene l'header condiviso da 12 byte seguito da **30 byte
Opus mono**: 42 byte totali, 400 pacchetti/s, **96 kbit/s di payload** e 134,4 kbit/s
incluso l'header JAM (esclusi UDP/IP). Il codec usa `Application::LowDelay` (CELT),
48 kHz, frame da **120 campioni / 2,5 ms** e bitrate CBR fisso. Un solo encoder
locale vive nel worker UDP; i callback hardware restano da 128 frame. Il ring
SPSC conserva i campioni residui tra i callback, senza padding o ricampionamento.
Sequenza e timestamp avanzano rispettivamente di 1 e 120 con wrap-around.

Il formato sostituisce il PCM di JAM-9: **tutti i client della stanza devono essere
aggiornati**, perché non esiste negoziazione del codec. Il server SFU resta
invariato e inoltra il payload senza interpretarlo. Il client scarta datagrammi
con lunghezza errata, header non valido, stream diverso dal primario, byte
riservato non zero, eco locale, durata Opus diversa da 120 campioni o audio stereo.
In cattura i campioni non finiti diventano silenzio e quelli fuori `[-1, 1]`
vengono limitati prima della codifica. Il decoder limita l'uscita a `[-1, 1]`.

Il jitter buffer per peer (JAM-10) riordina i pacchetti compressi prima della
decodifica; il mixer multicanale (JAM-11) riceve PCM tramite i ring dedicati.
Gli underflow di rete producono silenzio e vengono contati senza interrompere
la sessione; i problemi hardware `cpal` restano fatali. Gli overflow d'ingresso
vengono contati e scartati. Il loopback mantiene i suoi criteri rigorosi JAM-8.

I test automatici verificano retry, heartbeat, framing Opus, wrap-around,
validazione dei pacchetti, isolamento dell'endpoint UDP, timeout e `Leave`,
oltre allo scambio bidirezionale senza eco fra due worker e un vero `jam-server`
su localhost. Per completare l'accettazione hardware, avviare i due client su
PC o schede diverse e verificare all'ascolto che entrambi ricevano lo strumento
dell'altro in tempo reale. Questo ascolto non è sostituito dai test automatici.

### 9. Jitter buffer per peer e simulatore — JAM-10

Ogni peer remoto ha un `PeerJitterBuffer` separato nel worker UDP, con 64 slot
preallocati alla prima ricezione audio, insieme a un decoder Opus indipendente.
Il playback attende inizialmente **3 chunk (7,5 ms)** e riordina i numeri di sequenza, incluso il wrap-around `u32`.
`--jitter-chunks N` permette un ritardo iniziale da 1 a 16 chunk. Questo ritardo
si aggiunge al prefill del ring audio e ai buffer del dispositivo.

I frame duplicati già in coda e quelli tardivi vengono scartati. Tre frame tardivi
recenti e distinti fanno crescere il target di un chunk, fino a 8 (20 ms);
un valore iniziale esplicito superiore a 8 viene rispettato. I duplicati tardivi
e i frame scartati intenzionalmente non alimentano questo adattamento.
L'aumento inserisce un frame PLC senza fermare il playback. Dopo ogni secondo senza pacchetti tardivi,
il target scende di un chunk verso il valore configurato. La coda è limitata
a target + 3 chunk, contando anche i buchi di sequenza;
i frame vecchi vengono scartati e il decoder del solo peer viene riallineato.
I frame mancanti vengono sintetizzati dal PLC del decoder Opus del peer.
Con il crate `opus`, `decode_float(&[], &mut output, false)` rappresenta un
pacchetto perso: il flag FEC resta disattivato, perché CELT non usa il FEC SILK.
Anche un payload corrotto rilevato in decodifica viene trattato come perdita.
Anche il frame di compensazione usa il PLC Opus senza consumare i pacchetti accodati.
I piccoli riallineamenti fanno avanzare il decoder senza azzerarne la storia.
Una grande discontinuità di sequenza reinizializza coda e decoder del solo peer
interessato; gli altri decoder conservano la propria storia.
La lista peer e un timeout di 5 secondi eliminano gli stati non più attivi.
Con JAM-25 ogni pacchetto audio valido aggiorna l'attività del peer, compresi
duplicati e pacchetti tardivi: il timeout misura il silenzio di rete.
Se il buffer è vuoto, 8 sequenze consecutive tardive riancorano il playout
all'ultimo pacchetto ricevuto e ricostruiscono il margine target, anche al limite
adattivo di 20 ms. Per target manuali superiori a 13 chunk la soglia scende fino
a 5 pacchetti, così rilevamento e prefill restano entro 50 ms con arrivi regolari.
Il riaggancio ammette al massimo 400 frame di ritardo (1 secondo); oltre questa
soglia l'audio viene scartato perché troppo vecchio per una sessione musicale.
Il decoder conserva la storia per salti inferiori a 64 frame e viene resettato
da 64 frame in poi. Alla discesa dello spike il trim rilascia il backlog.
I test su clock simulato coprono scalini di 50, 200 e 600 ms per 7 secondi,
recupero entro 50 ms, ritorno al target e conservazione delle statistiche peer.

Il worker inserisce tutti i pacchetti maturati prima di avanzare il playout.
Normalmente produce un chunk per scadenza (2,5 ms); dopo un ritardo recupera i chunk
maturati entro lo spazio libero del ring SPSC dedicato, al massimo 64 per chiamata.
Se non entra un chunk intero, conserva pacchetti e scadenze per il prossimo risveglio,
senza avanzare il decoder né contare perdite. Il trim limita l'arretrato reale a
target + 3 chunk, oltre ai chunk consegnabili nella chiamata corrente. Solo una
sospensione oltre la finestra di riordino (160 ms) fa scadere i tick più vecchi.
Il prefill resta indipendente per peer e la fase del clock viene conservata.
Il mix avviene nel callback audio
come descritto in JAM-11. I callback `cpal` restano senza allocazioni, lock o I/O.

Per provare perdita e jitter in ricezione su uno o entrambi i client:

```bash
cargo run --locked --release -p jam-client -- --server 127.0.0.1:7777 --nickname chitarra --jitter-chunks 3 --simulate-loss 0.02 --simulate-jitter-ms 10
```

`--simulate-loss` accetta una probabilità fra 0 e 1; `--simulate-jitter-ms`
accetta 0..=1000 e aggiunge a ciascun frame audio un ritardo casuale uniforme
fra zero e il valore scelto, senza sospendere il worker. I messaggi di controllo
non vengono alterati. Il simulatore è disattivato per default e la sua coda è
limitata a 512 frame, scartando ulteriori ingressi quando piena.

I test coprono riordino 102/101, duplicati, ritardi, wrap-around, compensazione
delle perdite, isolamento dei peer, scadenza e cadenza con 2% di perdita e 10 ms
di jitter su tempo simulato. Per l'accettazione all'ascolto usare due client con
uno strumento o parlato continuo e verificare intelligibilità e assenza di
interruzioni con questi flag; tale verifica hardware rimane manuale.

### 10. Mixer multicanale RT e soft clipping — JAM-11

In modalità UDP, il callback di output legge un array fisso di `MAX_PEERS` ring
`rtrb` indicizzati con `peer_id - 1` e allocati prima dell'avvio degli stream.
Con JAM-36, `mixer::channels(buffer_frames)` dimensiona ogni ring sul blocco hardware:
la riserva è di 240 campioni (5 ms) sotto 128 frame; da 128 frame vale
`max(240, callback_frames * 2 + 120)`.
Il prefill richiede blocco + riserva; la capacità arrotonda questa soglia a chunk
interi da 120 campioni, senza aggiungere un ulteriore chunk. Il callback conserva la coda
che ha completato il prefill, senza tagliarla subito dopo lo startup.

| Blocco hardware | Prefill minimo | Primo prefill a chunk interi | Capacità SPSC |
| --- | --- | --- | --- |
| 64 | 304 | 360 | 360 |
| 128 | 504 | 600 | 600 |
| 256 | 888 | 960 | 960 |

I valori sono campioni mono, indipendenti dal numero di canali hardware.
Il jitter buffer ammette un solo chunk oltre il target; dopo 250 ms senza pacchetti
in ritardo riduce di un chunk il target adattivo, fino al valore CLI richiesto
(3 chunk predefiniti). A 64 frame il budget stimato di ricezione è quindi al massimo
15 ms in regime stabile; l'occupazione reale oscilla per la quantizzazione di chunk
e callback. I test verificano anche la media reale entro 15 ms con jitter sotto 3 ms.
La riserva fissa non copre stalli del worker da 15 ms: in quel caso sono attesi
underflow, seguiti da refill e recupero senza aumentare permanentemente il buffering.
I test a 64/128/256 frame distinguono questi stalli dalla cadenza normale 0,25–1,5 ms.
La capacità limita la latenza anche con callback ferme: il worker trattiene i
chunk nel jitter buffer invece di estrarli e perderli contro una SPSC piena.
Il worker conserva un jitter buffer indipendente per peer e limita i campioni decodificati senza sommare i flussi. Il callback somma `sample * gain`
e applica `tanh(sum)`, duplicando il risultato mono sui canali hardware.
Con quattro flussi da `0.8`, l'uscita è circa `0.9967` e resta in `[-1, 1]`.
Il soft clipping modifica anche i segnali sotto soglia, secondo la curva `tanh`.
Il loopback locale conserva il percorso originale senza soft clipping.

`Mixer::controls` espone un `Arc<[Controls; MAX_PEERS]>` condivisibile con una
futura UI. Ogni slot ha `gain: AtomicU32` (`f32::to_bits`, default `1.0`),
`mute: AtomicBool` e `solo: AtomicBool` (default `false`). I parametri vengono
letti per ogni frame hardware; mute prevale su solo. Se almeno un peer attivo
è in solo, gli altri sono silenziati. I gain non finiti vengono silenziati;
la somma in `f64` evita overflow anche con gain `f32` finiti molto grandi.
I controlli sono disponibili come API interna; questo ticket non aggiunge una UI.

Anche i peer silenziati vengono consumati, così l'unmute non riproduce audio
arretrato. Un ring vuoto contribuisce silenzio senza bloccare gli altri peer.
Alla rimozione per lista peer o timeout, un contatore di generazione atomico
invalida i campioni già accodati e ripristina i controlli ai default: un ID
riutilizzato non eredita l'audio della sessione precedente.

I test coprono quattro flussi simultanei, continuità della curva, gain/mute/solo
su audio già accodato, stereo, underflow, overflow indipendente, riuso ID e zero
allocazioni nel callback anche durante il ricambio dei peer. La conferma
all'ascolto con più musicisti resta una verifica hardware manuale.

### 11. Compilazione e verifica Opus — JAM-12

Il crate `opus` usa `opusic-sys`, che compila la copia inclusa di libopus:
servono **CMake e un compilatore C/C++** oltre ai prerequisiti audio di `cpal`.
Non occorre installare separatamente libopus sul sistema di esecuzione.

I test verificano configurazione LowDelay/CBR, payload di 30 byte, durata mono,
qualità del round-trip su un tono, passaggio 128 → 120 senza perdere campioni,
riordino prima della decodifica, PLC su una perdita e su perdite consecutive,
ripresa e wrap-around, reset e isolamento dei decoder per peer. Restano coperti
il simulatore al 2% di perdita / 10 ms di jitter, lo scambio bidirezionale e quattro
peer remoti attraverso un vero SFU, oltre alle zero allocazioni dei callback.
La verifica percettiva su strumenti e hardware reali resta manuale: usare due
client aggiornati, prima senza simulatore e poi con i flag della sezione 9.

### 12. Setup hardware e connessione desktop — JAM-13

```bash
cargo run --locked --release -p jam-client
```

L'avvio senza argomenti apre la finestra `eframe`/`egui`. Gli argomenti CLI
continuano ad attivare le modalità diagnostiche delle sezioni precedenti.
Su Linux la GUI richiede una sessione X11 o Wayland, OpenGL e le librerie di sviluppo
del sistema finestra; su Debian/Ubuntu installare `libxkbcommon-dev`,
`libwayland-dev`, `libx11-dev`, `libxcursor-dev`, `libxi-dev`, `libxrandr-dev`
e `libgl1-mesa-dev`, oltre ai prerequisiti audio e Opus già descritti.

Scegliere backend, ingresso e uscita (anche schede USB/driver ASIO), nickname e
server come `IP:porta` o `[IPv6]:porta`. Il menu buffer offre **64, 128, 256 frame**
a 48 kHz, abilitando solo le dimensioni comuni ai due dispositivi. Se il driver
non comunica il range, la costruzione dello stream verifica la richiesta.
Un dispositivo salvato ma scollegato resta segnalato come non disponibile;
non viene sostituito automaticamente. `Aggiorna dispositivi` ripete la ricerca.

`Connetti` avvia audio e handshake UDP; lo stato diventa connesso solo dopo
`JoinAccepted`. Un server che non accetta il join entro 5 secondi, un errore
hardware o un timeout riportano la schermata allo stato disconnesso con un errore.
`Disconnetti` funziona anche durante l'handshake, arresta gli stream, invia `Leave`
se il join era completato e rilascia socket, codec e ring. È possibile cambiare
hardware e riconnettersi senza riavviare. Usare le cuffie: il monitoraggio locale
software è attivo durante la sessione.

Le preferenze sono salvate automaticamente in TOML dopo le modifiche e alla
chiusura, e ricaricate al successivo avvio. Il percorso è mostrato nella finestra:
normalmente `$XDG_CONFIG_HOME/virtual-jam/client.toml` (o
`~/.config/virtual-jam/client.toml`) su Linux, la directory Application Support
su macOS e AppData su Windows. `JAM_CLIENT_CONFIG` consente di specificare un
file alternativo. Gli errori di lettura/scrittura sono visibili nella GUI; un
file non leggibile o malformato non viene sostituito finché non si modificano
le preferenze o si preme `Connetti`.

`AppState` appartiene alla UI. I comandi `Connect`, `Disconnect`, `SelectDevice`
e la persistenza passano via `crossbeam_channel` al controller non real-time che
possiede stream e rete UDP. I canali non entrano nei callback `cpal`, che
mantengono esclusivamente ring `rtrb` e atomici. I test coprono persistenza,
validazione, selezione dei buffer, cicli di connessione, rilascio delle sessioni,
recupero dagli errori e timeout/`Leave` UDP. La selezione di una scheda USB fisica
e l'ascolto con due musicisti richiedono una verifica manuale sull'hardware.


### 13. Mixer visivo — JAM-14

Dopo la connessione, “Il tuo mix in cuffia” mostra una strip per ogni musicista
remoto presente nella lista della stanza, anche prima che invii audio. Le strip
appaiono e scompaiono agli aggiornamenti della lista; la disconnessione le rimuove.

Ogni strip mostra nickname, fader verticale da −60 a +6 dB (0 dB = gain 1),
Mute e Solo. Più Solo possono essere attivi insieme; Mute prevale su Solo.
−60 dB attenua il segnale a 0,001: per silenziarlo completamente usare Mute.
Le modifiche agiscono solo sul proprio ascolto, tramite gli atomici del mixer.
La strip «Ingresso locale» mostra il picco di cattura (JAM-15) e gli stessi
controlli per il monitoraggio in cuffia (JAM-18). Fader e Mute locali non modificano
il segnale inviato via UDP. Solo partecipa al mix insieme ai Solo dei peer: un Solo
locale esclude i peer senza Solo e un Solo remoto esclude l'ingresso senza Solo.
Le impostazioni dei peer sono temporanee e tornano ai valori iniziali alla loro
uscita o alla riconnessione.

Verifica manuale: connettere due o più client, variare i fader, attivare Solo e
Mute, poi disconnettere e riconnettere un peer verificando strip e reset dei
controlli. I test automatici coprono conversione dB/gain, aggiornamenti della
lista, reset dei peer e applicazione dei controlli sull'audio già accodato.


### 14. Telemetria e VU meter — JAM-15

Le strip mostrano il picco dell’ingresso locale e di ogni peer prima di fader,
mute e solo. La scala è −60…0 dBFS, verde sotto −12 dBFS, gialla fino a −3 dBFS,
rossa sopra. Il marker trattiene il picco per un secondo; a 0 dBFS diventa rosso.
La UI richiede aggiornamenti a 60 fps e applica un decadimento temporale.
I callback calcolano un massimo per blocco su stack e lo accumulano in
`AtomicU32`; la UI lo legge e azzera, senza perdere i picchi tra due repaint.
Non vengono aggiunti lock, allocazioni o chiamate all’orologio nei callback.

La barra superiore riceve metriche ogni 500 ms dal thread di rete:

- **Ping RTT**: tempo di andata e ritorno al server, misurato dal Ping/Pong
  corrispondente; `—` prima del primo campione e dopo cinque secondi senza misura.
- **Jitter**: media esponenziale della variazione dei tempi di transito
  (fattore 1/16), usando sequenze e frame da 2,5 ms; mostra il massimo tra peer.
- **Packet Loss**: percentuale cumulativa in ricezione sui peer attivi, pesata
  per pacchetti attesi. Una finestra di 64 pacchetti permette il riordino prima
  di confermare la perdita; i duplicati non alterano il conteggio. La misura
  inizia dal primo pacchetto ricevuto e richiede pacchetti successivi per
  confermare i buchi; non misura la perdita in trasmissione verso il server.
- **Jitter Buffer**: massimo tra peer dei millisecondi di audio codificato
  effettivamente accodato, esclusi coda SPSC e buffer hardware.

Il simulatore JAM-10 agisce prima delle misure. I test verificano perdita al 2%
con jitter simulato di 10 ms, RTT, cadenza di pubblicazione, riordino, duplicati,
wrap-around, indipendenza dei meter e assenza di allocazioni audio.
Per la verifica manuale, connettere due client GUI, suonare segnali di intensità
variabile e controllare movimento fluido, marker di picco, mute/fader e reset
alla riconnessione. La fluidità effettiva a 60 fps e la sincronia con una scheda
audio fisica richiedono questa prova sul desktop.

### Budget di latenza end-to-end — JAM-31

La barra di stato mostra «Latenza E2E: ~X.X ms» con tre fasce: verde sotto
20 ms, giallo da 20 a 25 ms inclusi, rosso oltre 25 ms (rischio tempo-drag).
Il tooltip scompone la stima one-way in audio I/O locale (buffer di cattura
richiesto più ultimo blocco di output osservato, con fallback al buffer richiesto),
frame Opus da 2,5 ms più lookahead interrogato sull'encoder, RTT server / 2 e
massimo audio codificato accodato tra peer più riserva SPSC del mixer.
Il margine rispetto a 25 ms può essere negativo. La riserva SPSC è di 5 ms a
64 frame e cresce con il blocco di output osservato da 128 frame in su.

È un budget stimato a 48 kHz: RTT server / 2 è un'approssimazione della rete,
non una misura del percorso audio completo tra musicisti; la configurazione
hardware remota e le latenze interne del driver non sono misurate. Senza RTT
valido, anche dopo la scadenza della misura, totale, margine e classificazione
restano indisponibili. Il calcolo avviene nel controller ogni 500 ms, senza
modifiche ai callback audio. Il logger `telemetry-log` registra le componenti,
totale, margine e classe numerica (0 ottimale, 1 limite, 2 tempo-drag) anche nel
riepilogo; i valori indisponibili sono `N/A` ed esclusi dalle statistiche.

Verifica manuale: durante una sessione aprire il tooltip, controllare che le
componenti sommino al totale e che il margine sia `25 - totale`; confrontare
con il log della stessa sessione e verificare il reset alla disconnessione.

### Release beta e installer

Gli installer desktop sono disponibili nelle [release GitHub](https://github.com/mielecarmine/Virtual-Jam-Hub/releases).
La beta `1.0.0-beta-1` copre Windows x64 (`.exe`), Linux amd64 (`.deb`)
e macOS Intel/Apple Silicon (`.pkg`). Requisiti e limiti sono descritti nelle
[note della beta](packaging/RELEASE-NOTES.md).

La pipeline `.github/workflows/release.yml` esegue fmt, clippy, test, build release
e installazione di prova su ogni piattaforma. I branch `codex/release-*` producono
artefatti di verifica; un tag `1.*-beta-*` uguale alla versione Cargo pubblica la
prerelease solo dopo il successo di tutte e quattro le build, con checksum SHA-256.

### 15. Monitoraggio locale in cuffia — JAM-18

La callback di cattura invia il primo canale sia alla rete sia a un ring SPSC
`rtrb` dedicato al mixer, senza passare dal worker UDP, dal codec o dal jitter buffer.
Il volume iniziale è 0 dB, con Mute e Solo disattivati. Il mixer non attende un
prefill locale: riproduce subito i campioni disponibili su tutti i canali di uscita,
applica gain/Mute/Solo e il soft clipping comune al mix. I controlli si azzerano
alla nuova sessione; il meter resta prima del fader.

A ogni callback di uscita il consumer conserva al massimo una callback di campioni
recenti (64/128/256 frame nella GUI, circa 1,3/2,7/5,3 ms a 48 kHz, oltre ai buffer
hardware). Le posizioni dei campioni avanzano anche quando il ring è pieno:
dopo un ritardo dell'output, l'audio vecchio viene scartato con lavoro limitato alla
capacità del ring. Se tutti i campioni disponibili sono stantii, l'uscita resta
silenziosa fino alla nuova cattura. Mute e Solo continuano a consumare il ring.
Nessun lock, allocazione o I/O viene aggiunto ai callback audio.

I test coprono gain/Mute/Solo locali e remoti, indipendenza dal PCM destinato alla
rete anche in overflow, partenza senza peer, underflow, valori non finiti,
recupero dopo output ritardato, wrap delle posizioni e zero allocazioni.
Verifica manuale con scheda audio e cuffie: collegare lo strumento, connettersi,
regolare la strip «Ingresso locale» e verificare con un secondo musicista che il
segnale ricevuto resti invariato mentre si modifica il proprio ascolto.

### Benchmark di rete headless (JAM-19)

Il benchmark usa il motore `Network` del client, senza aprire dispositivi `cpal`:
handshake UDP, Ping periodici, encoder Opus LowDelay mono a 48 kHz, frame da 120
campioni ogni 2,5 ms e payload CBR da 30 byte. Ogni peer riceve e decodifica
con jitter buffer indipendenti; il mixer consuma l'audio in memoria.

Avviare un `jam-server` locale o usare l'indirizzo di un server remoto:

```bash
cargo run --release -p jam-server
cargo run --release -p jam-client --example net_bench -- 127.0.0.1:7777 --peers 3 --seconds 10 --output report.txt
```

Le opzioni predefinite sono `--peers 2` e `--seconds 10`. Sono ammessi da 2 a 8
peer e almeno 2 secondi, per raccogliere anche le risposte ai Ping. Il server
deve avere abbastanza posti liberi. `--output` è opzionale e richiede un percorso
con estensione `.txt`; `--help` mostra la sintassi.

La durata misurata inizia dopo l'accettazione di tutti i peer. I campioni di
telemetria pubblicati ogni 500 ms vengono conservati per peer e aggregati in un
report `min / avg / p95 / max` per Ping RTT, jitter, packet loss e jitter buffer.
Il p95 usa il rango più vicino (`ceil(0,95 * n)`); RTT non ancora disponibili sono
esclusi e l'assenza di campioni è indicata con `N/A`. Jitter e buffer di ogni
snapshot sono i massimi tra i peer remoti, come nella GUI; il buffer non include
le code SPSC o hardware. Il packet loss usa la finestra di riordino del motore
esistente, quindi non è una misura delle ultime code ancora in volo.

Il report include numero di snapshot, frame sintetici e presenza di audio
decodificato per ogni peer. I frame saltati per ritardi del processo sono
riportati separatamente: non vengono recuperati con raffiche di pacchetti.
Il benchmark misura anche gli effetti dello scheduling della macchina che lo
esegue; usare una macchina scarica e la build release per confronti riproducibili.
Alla chiusura normale e in caso di errore i peer accettati inviano `Leave` (UDP
best effort). Il report non misura la latenza audio end-to-end di dispositivi fisici.

Il test localhost a tre peer e i test di aggregazione vengono eseguiti anche da
`cargo test --workspace`, oppure con `cargo test -p jam-client --example net_bench`.

### Logging delle sessioni desktop di pre-release — JAM-20

Gli installer di pre-release abilitano automaticamente la feature `telemetry-log`
quando la versione Cargo contiene un suffisso SemVer (alpha, beta, rc o altro).
Le build standard e gli installer con versione stabile compilano senza logger.
Per provarlo localmente:

```bash
cargo run -p jam-client --features telemetry-log
```

Ogni connessione dalla GUI crea un file `telemetry-*.txt` accanto a `client.toml`.
Per scegliere una directory di prova, impostare `JAM_CLIENT_CONFIG` al percorso
completo del file di configurazione, ad esempio `/tmp/jam-test/client.toml`.
Il thread dedicato `jam-telemetry` apre, formatta, scrive e scarica i file,
e calcola il riepilogo finale. `jam-controller` consegna snapshot con timestamp
di acquisizione tramite una coda lock-free SPSC `rtrb` da 64 campioni (32 secondi):
il campionamento non alloca, non attende il disco e non risveglia il logger con syscall.
Se la coda è piena viene scartato il nuovo campione; `telemetry_dropped_samples`
riporta il totale saturante nel file prima del riepilogo. Le statistiche includono
solo i campioni accettati. Il logger controlla la coda ogni 10 ms, senza richiedere
una priorità audio. Le callback audio e il worker CLI `udp-network` non scrivono log.
Il percorso diagnostico CLI e il benchmark headless mantengono i propri report.

L'header riporta versione, OS, backend e dispositivi effettivamente aperti,
buffer hardware richiesto, canali e frequenze degli stream CPAL, dimensione
dell'ultimo blocco di output osservato (`N/A` se nessuna callback) ed endpoint.
I campioni ogni 500 ms includono peer
remoti (escluso il client locale), RTT, jitter, perdita, buffer di rete e contatori
cumulativi di overflow/underflow in campioni audio. Un RTT non disponibile è
`N/A` ed è escluso dalle statistiche. Il buffer di rete esclude code hardware/SPSC.
La diagnostica JAM-26 aggiunge i contatori cumulativi di callback input/output,
campioni silenziati per peer attivi non pronti e campioni scartati dall'allineamento
(incluse le vecchie generazioni). I campioni sono mono, sommati sui peer, anche
quando l'uscita è stereo. `last_output_frames` riporta l'ultimo blocco valido del driver.
I contatori del worker aggregano i peer e conservano i totali dopo timeout/uscita:
`late_dropped` include `late_dropped_far` (ritardo di almeno 64 frame),
`plc_frames` conta le decodifiche PLC (anche hold, fallback da payload corrotto e
frame mancanti durante skip), `skipped_frames` gli avanzamenti skip/trim,
`rx_push_dropped` i chunk interi rifiutati dalla coda e `resyncs` i riancoraggi/reset
del cursore, esclusa la creazione iniziale. I contatori audio u32 possono fare wrap;
quelli del worker saturano. La dimensione del blocco è una misura, non un contatore.
`worker_max_gap_ms` misura il massimo intervallo tra tick consecutivi del worker;
`rtt_max_ms` il massimo dei nuovi RTT ricevuti nell'intervallo (`N/A` senza Pong).
Entrambi i picchi ripartono a ogni pubblicazione da 500 ms; l'ultimo RTT resta
separato e può comparire su più righe.

Alla disconnessione, errore di sessione o chiusura ordinata, audio e rete vengono
fermati prima di attendere lo scarico della coda e la chiusura del logger;
un disco bloccato può quindi ritardare la chiusura, senza bloccare audio attivo.
Il riepilogo riporta
minimo, media, p95 (nearest rank) e massimo di tutti i campioni della sessione.
Le sessioni più brevi del primo intervallo possono avere zero campioni.
Gli arresti forzati mantengono i campioni già scaricati, ma non il riepilogo finale.
Le statistiche esatte conservano i campioni in memoria per la durata della sessione.
Un errore di apertura/scrittura viene segnalato senza interrompere l'audio;
un errore durante la finalizzazione viene riportato su stderr.

### Micro-underflow e riaggancio del mixer — JAM-30

Ogni peer conserva uno stato di continuità indipendente. Una lettura mancante
sfuma l'ultimo campione verso zero in 48 campioni (1 ms a 48 kHz); il ritorno
dell'audio interpola dall'ultimo valore emesso al segnale corrente per 48 campioni.
Le rampe continuano attraverso i confini delle callback. Dopo meno di 120 campioni
mancanti consecutivi (2.5 ms) il peer resta pronto e può riprendere alla prossima
lettura disponibile, senza attendere nuovamente 904–1096 campioni di prefill.
I campioni mancanti costituiscono un debito: al ritorno vengono scartati altrettanti
campioni dal ring prima di consumare audio valido, evitando di accumulare latenza.
Questi scarti incrementano `align_dropped_samples`.

Dopo 120 letture mancanti consecutive si ricostruisce la riserva completa;
`output_underflows` conta le letture mancate durante lo stato pronto,
`not_ready_samples` conta i campioni in attesa del prefill. Il debito viene azzerato
al riaggancio completo; il limite di coda di `align` resta attivo. Rimozione/riuso
di un ID azzera anche lo stato delle rampe, senza propagare audio del vecchio peer.
Il primo avvio mantiene il prefill esistente. Mute, solo e gain si applicano anche
all'audio di raccordo. Lo stato è in array fissi, senza heap, lock o syscall.

Verifica dell'avvio a 256 frame: nel log macOS
`telemetry-1795015098111620000-2011-0.txt`, a 0.345 s sono già presenti
`resyncs=1`, `late_dropped=62`, `plc_frames=66`, ma `output_underflows=0` e
`align_dropped_samples=0`; a 0.845 s `late_dropped_far=34` mentre gli underflow
restano zero. Nel log Windows `telemetry-1795015097159872200-21532-0.txt`
`resyncs=1` compare a 0.975 s e `late_dropped_far=34` a 1.975 s, sempre con
underflow e scarti del mixer a zero. I contatori del jitter buffer non sono prova di un underflow
del mixer: `late_dropped_far` richiede un pacchetto almeno 64 sequenze (160 ms)
dietro al cursore; `resyncs` può contare riordino iniziale, salti di sequenza o
riancoraggi. I log aggregati non identificano la causa esatta dei 34 scarti.

Il test `startup_at_256_frames_does_not_create_far_late_packets_or_resyncs`
simula due secondi di arrivi regolari, playout del worker e callback da 256 frame
con quattro fasi iniziali: underflow, scarti lontani e resync restano zero.
Non riproduce le condizioni di rete/driver dei log: per la verifica hardware
confrontare Windows/ASIO e macOS/CoreAudio a 256 frame, raccogliendo i log fin
dall'aggancio e ascoltando le transizioni. I test del mixer coprono inoltre
micro-underflow ripetuti, rampe tra callback, refill per peer e assenza di
accumulo di latenza; `audio::tests::allocation` copre i nuovi percorsi.

### Priorità del controller e verifica degli stalli — JAM-29

Il thread `jam-controller` richiede MMCSS `Pro Audio` con `AVRT_PRIORITY_HIGH`
su Windows e QoS `QOS_CLASS_USER_INTERACTIVE` su macOS. La registrazione è locale
al thread, viene rilasciata alla sua uscita e non modifica la priorità del processo
o il timer globale. Su macOS il logger imposta esplicitamente QoS utility per non
ereditare quella del controller. Un errore di scheduling viene segnalato alla GUI
e la sessione continua con la priorità disponibile. Linux mantiene lo scheduling
standard. Riferimenti: [MMCSS](https://learn.microsoft.com/en-us/windows/win32/procthread/multimedia-class-scheduler-service),
[QoS pthread Apple](https://developer.apple.com/library/archive/documentation/Performance/Conceptual/EnergyGuide-iOS/PrioritizeWorkWithQoS.html).

I test coprono logger fermo, saturazione senza allocazioni sul produttore,
errori asincroni di apertura/scrittura, timestamp di acquisizione e scarico finale.
La prova di scheduling headless opzionale stampa i gap reali del controller:

```bash
cargo test --release -p jam-client --features telemetry-log controller_scheduling_probe -- --ignored --nocapture
```

Per accettare il risultato sulle postazioni reali, ripetere la verifica hardware
qui sotto su Windows/ASIO e macOS/CoreAudio, mantenendo audio continuo per almeno
5 minuti per buffer. Confrontare con la build precedente su macchine scariche:
`worker_max_gap_ms` deve restare normalmente sotto 5 ms, senza picchi sopra 10 ms
riconducibili ai flush periodici; verificare `telemetry_dropped_samples=0`.
La prova headless non verifica i driver audio né sostituisce questi log hardware.

La pipeline verifica entrambe le configurazioni. I test del logger si eseguono con:

```bash
cargo test --workspace --features jam-client/telemetry-log
```

### Verifica scheduling e continuità — JAM-24

Durante una sessione desktop attiva su Windows, il controller controlla i comandi
senza bloccare e attende 250 µs con `std::thread::sleep`. Su Windows 10 1803+
Rust usa un waitable timer ad alta risoluzione, evitando il timeout di canale
basato sul tick di sistema. A sessione inattiva rimane l'attesa bloccante da 50 ms.
Riferimento: [implementazione Rust Windows](https://github.com/rust-lang/rust/blob/1.85.0/library/std/src/sys/pal/windows/thread.rs).
Il sistema operativo può comunque ritardare un thread sotto carico; le simulazioni
coprono gap ripetuti da 5 a 15 ms, blocchi da 64/128/256 frame e peer indipendenti,
verificando assenza di underflow, silenzio successivo allo startup e scarti interni.

La verifica hardware richiede due postazioni Windows ASIO e macOS CoreAudio:

1. Avviare la build con `telemetry-log` su entrambe e connettersi alla stessa stanza.
2. Ripetere una sessione con audio continuo per ogni buffer: 64, 128 e 256 frame.
3. Su Windows passare dalla GUI in primo piano a un'altra finestra e poi minimizzare.
4. Confrontare i log dopo il prefill e fuori dalle riconnessioni: nessun incremento
   di `output_underflows`, `not_ready_samples` o `rx_push_dropped`;
   `output_callbacks` continua a crescere anche a 256 frame. Controllare
   `worker_max_gap_ms` per escludere una cadenza sistematica da 15,625 ms.
5. Verificare all'ascolto la continuità e annotare eventuali scarti di allineamento,
   frame saltati e picchi di scheduling/rete insieme ai log di entrambe le postazioni.

Questa prova non è sostituita dai test su clock simulato o da una sessione localhost.

### Clock e metronomo di stanza — JAM-32

Il controllo UDP include `MetronomeUpdate` (tipo `0x08`, 8 byte) e
`MetronomeState` (tipo `0x09`, 16 byte), entrambi little-endian e senza
allocazioni durante la codifica e la decodifica. La configurazione contiene
`enabled` (0/1), `bpm` (30–300), `beats_per_bar` (1–16) e `leader_peer_id`
(0 = clock di stanza; 1–8 = peer attivo). Il default è spento, 120 BPM, 4/4,
senza leader. Lo stato aggiunge `anchor_server_timestamp_us`, il beat zero
nel clock monotono del server.

Ogni membro può modificare il metronomo. Il server assegna una nuova ancora
all'attivazione e al cambio BPM; retry identici, spegnimento, cambio metrica
o leader conservano l'ancora. Invia lo stato a tutti i membri dopo ogni comando
valido e al nuovo peer dopo il join. Uscita e timeout del leader ripristinano
automaticamente il clock di stanza e notificano i superstiti, conservando la fase.
I client ignorano i comandi come conferme: lo stato locale cambia solo alla
ricezione dello stato autorevole.

`Network::clock_offset_us()` restituisce server meno clock locale (microsecondi
dall'avvio della connessione), inizialmente `None`. Per ogni Pong corrispondente
al Ping pendente, il filtro calcola `server_timestamp_us - (invio_locale + RTT/2)`
e sceglie il campione a RTT minimo degli ultimi 32 Pong validi, preferendo il
più recente a parità di RTT. La finestra mobile elimina gradualmente i vecchi
campioni per seguire variazioni di percorso e deriva del clock. Spike asimmetrici
vengono filtrati quando la finestra contiene campioni con RTT inferiore;
l'asimmetria permanente del percorso resta un limite della stima.
`server_timestamp_us(Instant)` converte nel clock di stanza;
`metronome_state()` legge lo stato e `update_metronome(config)` invia un comando
dopo il join. Queste API preparano sintetizzatore e GUI dei ticket JAM-33/JAM-34:
JAM-32 non genera audio del click e non lo trasmette in rete.

I test coprono layout e limiti dei nuovi messaggi, broadcast, join, reset del
leader e riuso degli ID, oltre a offset positivi/negativi, jitter asimmetrico,
Pong duplicati/non corrispondenti e ricambio della finestra del filtro.

### Sintetizzatore locale del metronomo — JAM-33

`Mixer::playback` sintetizza il click a 48 kHz e lo somma al bus prima del
soft-clipping: downbeat a 1200 Hz, altri battiti a 800 Hz con ampiezza ridotta,
impulsi di 20 ms con decadimento esponenziale e bordi raccordati. La fase usa
aritmetica intera e un contatore campioni: anche BPM come 73 o 137 non accumulano
errori di arrotondamento. Un cambio di BPM, fase, metrica o abilitazione lascia
terminare l'impulso già iniziato; una nuova scadenza durante quell'impulso non
lo interrompe. Gain e mute locali hanno una rampa di 1 ms.

`RxChannels::metronome` espone l'`Arc<MetronomeControls>` con gain, mute e beat
corrente atomici. I controlli locali non modificano lo stato di stanza né il
segnale di cattura inviato via UDP; mute e solo dei canali audio non fermano il
click. Il worker pubblica configurazione e ancora in campioni soltanto dopo
aver ricevuto stato autorevole e stima del clock, aggiornandoli ai cambi di
stato/offset. Cambi di sola metrica o leader preservano l'epoca audio. La
callback legge uno snapshot atomico con un solo tentativo, conservando il
precedente se il worker sta scrivendo: nessun lock, attesa, allocazione o lettura
di `Instant` nel sintetizzatore.

La precisione al campione riguarda la cadenza locale. La correlazione iniziale
tra clock di stanza e cursore audio usa l'ultimo blocco renderizzato: fase
assoluta fra dispositivi, latenza hardware e deriva del clock audio richiedono
verifica su hardware reale e non sono misurate dai test deterministici.
La GUI e la compensazione Tempo Leader sono descritte sotto (JAM-34).

I test coprono periodi interi/frazionari, accenti, callback da 64/128/256 frame,
ancore future, transizioni durante un impulso, gain/mute locali, pubblicazioni
interrotte e integrazione con il clock del worker senza peer audio. La suite
`audio::tests::allocation` verifica anche il metronomo attivo e riconfigurato.

### Tempo Leader e controlli del click — JAM-34

Durante la connessione, la sezione **Metronomo & Prevenzione Tempo-Drag** permette
di attivare il click di stanza, scegliere 30–300 BPM, 1–16 battiti per misura
(incluse 2/4, 3/4, 4/4 e 6/8) e il riferimento: clock assoluto oppure un musicista
connesso, incluso sé stessi. I BPM contano gli impulsi del click anche in 6/8.
Ogni modifica viene mostrata dopo la conferma UDP del server e propagata agli altri
client. I pallini seguono il beat renderizzato, con un simbolo distinto sul downbeat.
Volume in dB e **Mute Click** regolano soltanto l'ascolto locale, senza fermare il
beat visivo. Con E2E stimata oltre 25 ms e metronomo di stanza spento compare un
suggerimento discreto per attivarlo o scegliere un Tempo Leader.

Il clock assoluto e il Leader mantengono offset zero. Il Follower sottrae alla
fase nominale un ritardo positivo in campioni: transito SFU completo stimato
(`RTT locale / 2` per l'uplink del Leader + `RTT locale / 2` per il downlink),
target adattivo stabile del `PeerJitterBuffer` del solo Leader, riserva SPSC scalata,
frame Opus più lookahead (5 ms con l'encoder attuale) e audio I/O locale
(cattura richiesta + ultimo blocco output osservato; 2,67 ms a 64 frame).
Il target stabile evita che l'inserimento o il consumo di ogni pacchetto sposti il click.
La stima viene
aggiornata sul worker; il callback legge un atomico una volta per blocco, senza
allocazioni, lock o syscall. Cambi di riferimento conservano l'ancora di stanza e
lasciano terminare un click già iniziato. Se RTT o audio del Leader non sono ancora
disponibili, le rispettive componenti valgono zero; restano riserva SPSC, codec e audio I/O.

Il ritardo è una stima: il protocollo non misura l'uplink del Leader né l'hardware
remoto: usa le misure locali come proxy per il Leader. Non garantisce allineamento
acustico esatto tra dispositivi; resta necessaria
una verifica in cuffia su hardware reale. I test coprono segno del ritardo, offset
zero del Leader, selezione della sola coda interessata, cambi di riferimento,
fase assoluta con epoche audio diverse, propagazione dei comandi fra due client
attraverso il server reale e zero allocazioni durante le variazioni di offset.
