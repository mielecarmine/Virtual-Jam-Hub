# Deploy Linux — JAM-7

Il server ascolta su UDP `0.0.0.0:7777`. Il protocollo attuale non offre
autenticazione della stanza o cifratura: l'endpoint pubblico è per prove di sviluppo.

## Installazione

Su Ubuntu/Debian con systemd e UFW, compilare con Rust >= 1.85 sulla macchina
target (oppure con una toolchain compatibile con architettura e glibc del target):

```sh
cargo build --locked --release -p jam-server
sudo apt-get install ufw
sudo sh deploy/install.sh target/release/jam-server
```

L'installer crea `jamserver` senza home e con shell `/bin/false`, installa un
eseguibile di proprietà root, abilita l'avvio al boot e riavvia il servizio.
È rieseguibile per aggiornare il binario. I limiti sono 128 MiB di memoria,
16 task e 1.024 file aperti; non si impone una quota CPU al percorso UDP.
`Restart=always` e `RestartSec=2` coprono crash e uscite del processo.
Un arresto esplicito con `systemctl stop` resta intenzionalmente fermo.

L'installer aggiunge `ufw allow 7777/udp` ma non abilita UFW su host esistenti:
controllare `sudo ufw status` e predisporre prima l'accesso amministrativo.
Aprire anche UDP 7777 nel security group cloud. Il traffico applicativo non
richiede porte TCP. Per una nuova VM gestita via SSM si può abilitare UFW con
default deny incoming e allow outgoing, senza ingressi SSH.

```sh
sudo systemctl status jam-server --no-pager
sudo journalctl -u jam-server -n 50 --no-pager
sudo systemctl show jam-server -p User -p Restart -p RestartUSec -p MemoryMax
sudo ss -lunp 'sport = :7777'
```

## Verifica da rete domestica

Dal checkout locale, sostituire `IP_PUBBLICO` con l'indirizzo della VM:

```sh
cargo run --locked -p jam-server --example probe -- IP_PUBBLICO:7777
```

Il probe attende `JoinAccepted` con sample rate corretto, invia un `Ping`,
verifica il timestamp del `Pong` e invia `Leave`. Ritenta ogni 500 ms per
circa 3 secondi e termina con errore se il server non risponde. Consuma uno
slot della stanza per la durata della verifica; con 8 peer presenti non può entrare.
La verifica su loopback non prova l'apertura del firewall cloud.

Per controllare la resilienza su una VM di sviluppo senza musicisti connessi:

```sh
sudo systemctl kill --signal=SIGKILL --kill-whom=main jam-server
sleep 3
sudo systemctl is-active jam-server
sudo systemctl is-enabled jam-server
```

Dopo un reboot ripetere stato e probe dall'esterno.

## Risorse AWS

Region: `eu-north-1`. Accesso amministrativo tramite Systems Manager;
il ruolo `virtual-jam-server-ssm` assume identità EC2 con la sola policy
`AmazonSSMManagedInstanceCore`. Nessuna credenziale statica nel servizio.
IMDSv2 obbligatorio e disco gp3 cifrato. Non sono necessari NAT Gateway,
load balancer o IP elastici per questa verifica.

L'IP pubblico assegnato automaticamente può cambiare dopo stop/start.
VM, disco, IPv4 pubblico e traffico possono consumare crediti o generare costi:
controllare AWS Settings > Billing. Arrestare la VM riduce il consumo compute,
ma conserva il disco. Per rimuovere tutto dopo le prove: terminare l'istanza
(il volume root è DeleteOnTermination), eliminare il security group dedicato,
rimuovere il ruolo dall'instance profile, eliminare il profilo, staccare
`AmazonSSMManagedInstanceCore` e infine eliminare il ruolo dedicato.
