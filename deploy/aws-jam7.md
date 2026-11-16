# Ambiente di sviluppo JAM-7

Creato il 16 novembre 2026 nel progetto AWS `984263476689`.

| Risorsa | Valore |
| --- | --- |
| Region / AZ | `eu-north-1` / `eu-north-1a` |
| Istanza | `i-037113c20a85fa4f0` — `virtual-jam-server` |
| Tipo / crediti CPU | `t3.micro` / standard |
| Sistema | Ubuntu 24.04 x86_64 |
| AMI Canonical | `ami-084df9e6ad4622caf` |
| Endpoint iniziale | `16.170.231.226:7777` UDP |
| VPC | `vpc-0301f662621e9cada` (default esistente) |
| Subnet | `subnet-0b64e3d8bbd38bdc6` |
| Security group dedicato | `sg-0510529c20c033084` |
| Volume root | `vol-0f10684f78fbdfb7c`, gp3 12 GiB, cifrato, DeleteOnTermination |
| Ruolo / instance profile | `virtual-jam-server-ssm` |

Il security group consente in ingresso solo UDP 7777 da `0.0.0.0/0`.
L'uscita è consentita per aggiornamenti, risposte UDP e gestione SSM.
Il ruolo ha trust per `ec2.amazonaws.com` e policy
`arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore`.
Il profilo è collegato all'istanza; l'agente SSM è stato rilevato Online.
Non sono state create chiavi SSH o regole in ingresso TCP.

Avvio iniziale tramite cloud-init: installazione di build-essential, curl e UFW,
firewall con deny incoming / allow outgoing, apertura 7777/udp e abilitazione.
Il deploy via SSM trasferisce i sorgenti del server, installa Rust 1.85.1 con
profilo minimal per l'utente `ubuntu`, compila con
`cargo build --locked --release -p jam-server` e invoca `deploy/install.sh`.
Il checkout di build è in `/opt/virtual-jam-src`.

Accesso amministrativo dalla console AWS Systems Manager oppure, con CLI
autenticata e plugin Session Manager installato:

```sh
aws ssm start-session --region eu-north-1 --profile virtual-jam \
  --target i-037113c20a85fa4f0
```

Il servizio applicativo non usa AWS SDK o credenziali. Per audit del ruolo,
verificare trust, policy collegate e instance profile dalla console IAM;
non occorre stampare credenziali dal metadata service.

Per cleanup usare i nomi e ID di questa tabella seguendo `deploy/README.md`.
Conservare VPC, subnet e Internet Gateway preesistenti.

## Evidenze di verifica

- Build nativa release con Rust 1.85.1 completata sulla VM.
- `systemd-analyze verify` completato senza errori.
- Servizio `active (running)`, utente `jamserver`, `Restart=always`, intervallo 2 s.
- UFW attivo e socket in ascolto su `0.0.0.0:7777` UDP.
- Reinstallazione rieseguita con successo sul servizio già attivo.
- Crash con SIGKILL: PID passato da 4753 a 4783, `NRestarts=1`, servizio attivo.
- Probe dal computer locale all'IP pubblico: `JoinAccepted`, peer 1, 48.000 Hz;
  `Pong` con `client_timestamp_us=42` corrispondente al Ping.
- Reboot EC2: servizio ripartito automaticamente e probe pubblico ripetuto con successo.
- Gate locali: fmt, Clippy workspace e all-targets, test workspace superati;
  gate release a 5 peer / 500 frame: massimo 59,792 µs (limite 200 µs).

SHA-256 del binario remoto:
`238d7adc11e9c10c09f40656cce9090749ebc4ee8b071444e460299672061b79`.

La VM viene mantenuta attiva per i prossimi ticket su richiesta dell'utente.
