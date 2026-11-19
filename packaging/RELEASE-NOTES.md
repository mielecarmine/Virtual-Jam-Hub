Pre-release del client Virtual Jam `1.0.4-beta`, preparata da `develop`.

## Download e installazione

| Sistema | Installer | Requisiti |
| --- | --- | --- |
| Windows | `Virtual-Jam-1.0.4-beta-windows-x64-setup.exe` | Windows 10/11 x64; installazione per utente, senza amministratore |
| macOS Apple Silicon | `Virtual-Jam-1.0.4-beta-macos-arm64.pkg` | macOS 11 o successivo |

macOS: aprire il pacchetto e avviare **Virtual Jam** da Applicazioni; consentire l'accesso al microfono.
Windows: eseguire il setup e avviare **Virtual Jam** dal menu Start.
`SHA256SUMS.txt` contiene gli hash degli installer.

Gli installer di questa beta non hanno una firma commerciale Windows/Apple né notarizzazione Apple. SmartScreen o Gatekeeper possono richiedere un'autorizzazione esplicita nelle impostazioni di sicurezza. La firma ad hoc del bundle macOS non certifica l'identità dell'autore.

## Novità rispetto a 1.0.3-beta

- Migliorata la continuità della riproduzione durante lo smaltimento degli arretrati e il recupero da micro-interruzioni (JAM-28, JAM-30).
- Isolato l'I/O della telemetria e migliorata la priorità del controller audio (JAM-29).
- Aggiunta una stima della latenza end-to-end (JAM-31).
- Aggiunti clock di stanza e metronomo condiviso, con sintesi locale e controlli nella GUI (JAM-32, JAM-33, JAM-34).

## Novità della 1.0.3-beta

- Preservato l'audio in ricezione durante le pause di scheduling del client (JAM-24).
- Migliorato il recupero dei jitter buffer vuoti dopo picchi di latenza e corretto il timeout di attività dei peer (JAM-25).
- Estesa la diagnostica audio e dei worker per analizzare le sessioni problematiche (JAM-26).
- Preferite le configurazioni di uscita stereo nella selezione del dispositivo audio (JAM-27).
- Installer disponibili per Windows x64 e macOS Apple Silicon ARM64.

## Funzionalità introdotte nelle beta precedenti

- Ridotti gli underflow periodici del mixer in ricezione: le code dei peer vengono precaricate e mantengono una riserva di riproduzione (JAM-21).
- Preservata la continuità del decoder Opus durante l'adattamento del jitter buffer, evitando salti nella cronologia del codec (JAM-22).
- Corretta la crescita della latenza del jitter buffer causata dai ritardi di scheduling del client, con gestione dei pacchetti in raffica e dei salti del clock di riproduzione (JAM-23).

- Latenza di ricezione limitata e recupero del clock di riproduzione per evitare accumuli di ritardo nelle sessioni (JAM-17).
- Monitoraggio diretto dell'ingresso locale in cuffia, con gain e mute dedicati senza modificare il segnale inviato ai peer (JAM-18).
- Log di telemetria delle sessioni GUI, abilitati negli installer di pre-release: file `telemetry-*.txt` accanto a `client.toml`, utili per segnalare problemi di audio e rete (JAM-20).
- Benchmark di rete headless multi-peer disponibile nei sorgenti come esempio `net_bench` per verificare il comportamento sotto carico (JAM-19).

## Funzionalità

- Client desktop con selezione backend e dispositivi audio, buffer e preferenze persistenti.
- Sessioni audio UDP tramite server SFU, codec Opus e jitter buffer indipendenti.
- Mixer per partecipante con gain, mute e solo; VU meter e telemetria di rete.
- Backend ALSA su Linux, WASAPI/ASIO su Windows e CoreAudio su macOS.

## Limiti della beta

Serve un server Virtual Jam compatibile, raggiungibile via UDP (porta predefinita 7777): inserire il suo indirizzo nella GUI. Il server non viene installato dai pacchetti desktop; le istruzioni per compilarlo e installarlo sono in `deploy/README.md` nei sorgenti allegati.
Usare cuffie e dispositivi che supportano 48 kHz. ASIO richiede il driver del produttore.
Il protocollo attuale non offre autenticazione o cifratura: utilizzare sessioni su reti fidate/VPN.
Le verifiche automatiche non sostituiscono le prove audio reali: compatibilità delle schede, latenza e ascolto multipiattaforma restano da validare dai beta tester.
