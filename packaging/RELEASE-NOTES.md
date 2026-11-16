Prima beta pubblica di Virtual Jam, preparata da `develop`.

## Download e installazione

| Sistema | Installer | Requisiti |
| --- | --- | --- |
| Windows | `Virtual-Jam-1.0.0-beta-1-windows-x64-setup.exe` | Windows 10/11 x64; installazione per utente, senza amministratore |
| Linux | `Virtual-Jam-1.0.0-beta-1-linux-amd64.deb` | Ubuntu 22.04 o successivo / Debian 12 o successivo, amd64, desktop X11/Wayland e OpenGL |
| macOS Intel | `Virtual-Jam-1.0.0-beta-1-macos-x86_64.pkg` | macOS 11 o successivo |
| macOS Apple Silicon | `Virtual-Jam-1.0.0-beta-1-macos-arm64.pkg` | macOS 11 o successivo |

Linux: installare con `sudo apt install ./Virtual-Jam-1.0.0-beta-1-linux-amd64.deb`.
macOS: aprire il pacchetto e avviare **Virtual Jam** da Applicazioni; consentire l'accesso al microfono.
Windows: eseguire il setup e avviare **Virtual Jam** dal menu Start.
`SHA256SUMS.txt` contiene gli hash degli installer.

Gli installer di questa beta non hanno una firma commerciale Windows/Apple né notarizzazione Apple. SmartScreen o Gatekeeper possono richiedere un'autorizzazione esplicita nelle impostazioni di sicurezza. La firma ad hoc del bundle macOS non certifica l'identità dell'autore.

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
