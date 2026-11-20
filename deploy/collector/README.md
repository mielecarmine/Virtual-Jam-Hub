# Deploy Servizio Ricevitore Centrale (`jam-collector`) — `185.198.245.40`

Servizio HTTP/1.1 leggero (zero dipendenze esterne) progettato per girare dietro reverse proxy HTTPS (Caddy o Nginx) su `185.198.245.40`.

## Struttura directory su disco

Tutti i file ricevuti vengono catalogati automaticamente per **data UTC** e **IP di provenienza** ricavati lato server (dalla connessione o dagli header fidati `X-Real-IP` / `X-Forwarded-For` quando il proxy è su loopback):

```text
/var/log/jam-collector/
└── <YYYY-MM-DD>/
    └── <source_ip>/
        ├── server-<timestamp_ms>-<session_id>.log
        └── client-<timestamp_ms>-<peer_id>.json
```

* Gli IP IPv6 vengono sanitizzati sostituendo `:` con `_`.
* `session_id` e `peer_id` vengono filtrati rigorosamente a `[A-Za-z0-9_-]` (max 32 caratteri), eliminando qualsiasi rischio di *path traversal*.

## Sicurezza & Anti-Abuse

* **Payload cap rigido:** `<= 256 KiB` per report client (`POST /api/v1/logs/client`), `<= 2 MiB` per log server (`POST /api/v1/logs/server`). Richieste senza `Content-Length` o con `Transfer-Encoding: chunked` vengono rifiutate (`411`), quelle oltre soglia ricevono `413`.
* **Rate limiting per IP:** default `30` richieste/minuto per IP sorgente (`429 Too Many Requests`).
* **Token beta opzionale:** impostare `JAM_COLLECTOR_BETA_TOKEN` in `/etc/virtual-jam/collector.env` per richiedere `X-Jam-Beta-Key: <token>` oppure `Authorization: Bearer <token>` (`401 Unauthorized` se assente/errato).
* **Retention e quota disco:** pulizia automatica delle directory `<YYYY-MM-DD>` più vecchie di `JAM_COLLECTOR_RETENTION_DAYS` (default `30` giorni) e taglio delle giornate più vecchie se si supera `JAM_COLLECTOR_MAX_DISK_MB` (default `2048` MiB).

## Installazione su `185.198.245.40`

```sh
cargo build --locked --release -p jam-collector
sudo sh deploy/collector/install-collector.sh target/release/jam-collector
```

Configurare opzionalmente il token beta in `/etc/virtual-jam/collector.env` e riavviare:

```sh
sudo systemctl restart jam-collector.service
```

Esporre il servizio in HTTPS tramite **Caddy** (`deploy/collector/Caddyfile`) o **Nginx** (`deploy/collector/nginx-jam-collector.conf`) e aprire `443/tcp` su UFW/firewall (`jam-collector` ascolta solo su `127.0.0.1:8080`).

## Verifica rapida

```sh
curl -fsS https://185.198.245.40/healthz
curl -i -X POST https://185.198.245.40/api/v1/logs/client \
  -H "Content-Type: application/json" \
  -H "X-Jam-Peer-Id: peer-1" \
  -H "X-Jam-Beta-Key: <TOKEN>" \
  --data '{"schema_version":1,"client_version":"1.0.5-beta","connection_mode":"p2p"}'
```
