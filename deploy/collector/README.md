# Central Log Collector Deployment (`jam-collector`) — `185.198.245.40`

Lightweight HTTP/1.1 service (zero external dependencies) designed to run behind an HTTPS reverse proxy (Caddy or Nginx) on `185.198.245.40`.

## On-Disk Directory Structure

All received files are automatically cataloged by **UTC date** and **source IP** determined server-side (from the socket connection or trusted `X-Real-IP` / `X-Forwarded-For` headers when the proxy is on loopback):

```text
/var/log/jam-collector/
└── <YYYY-MM-DD>/
    └── <source_ip>/
        ├── server-<timestamp_ms>-<session_id>.log
        └── client-<timestamp_ms>-<peer_id>.json
```

* IPv6 addresses are sanitized by replacing `:` with `_`.
* `session_id` and `peer_id` are strictly filtered to `[A-Za-z0-9_-]` (max 32 characters), eliminating any *path traversal* risk.

## Security & Anti-Abuse

* **Strict payload cap:** `<= 256 KiB` for client reports (`POST /api/v1/logs/client`), `<= 2 MiB` for server logs (`POST /api/v1/logs/server`). Requests missing `Content-Length` or using `Transfer-Encoding: chunked` are rejected (`411`), and oversized payloads receive `413`.
* **Per-IP rate limiting:** default `30` requests/minute per source IP (`429 Too Many Requests`).
* **Optional beta token:** set `JAM_COLLECTOR_BETA_TOKEN` in `/etc/virtual-jam/collector.env` to require `X-Jam-Beta-Key: <token>` or `Authorization: Bearer <token>` (`401 Unauthorized` if missing or invalid).
* **Retention and disk quota:** automatic cleanup of `<YYYY-MM-DD>` directories older than `JAM_COLLECTOR_RETENTION_DAYS` (default `30` days) and pruning of the oldest days when exceeding `JAM_COLLECTOR_MAX_DISK_MB` (default `2048` MiB).

## Installation on `185.198.245.40`

```sh
cargo build --locked --release -p jam-collector
sudo sh deploy/collector/install-collector.sh target/release/jam-collector
```

Optionally configure the beta token in `/etc/virtual-jam/collector.env` and restart:

```sh
sudo systemctl restart jam-collector.service
```

Expose the service over HTTPS via **Caddy** (`deploy/collector/Caddyfile`) or **Nginx** (`deploy/collector/nginx-jam-collector.conf`) and open `443/tcp` on UFW/firewall (`jam-collector` listens only on `127.0.0.1:8080`).

## Quick Verification

```sh
curl -fsS https://185.198.245.40/healthz
curl -i -X POST https://185.198.245.40/api/v1/logs/client \
  -H "Content-Type: application/json" \
  -H "X-Jam-Peer-Id: peer-1" \
  -H "X-Jam-Beta-Key: <TOKEN>" \
  --data '{"schema_version":1,"client_version":"1.0.5-beta","connection_mode":"p2p"}'
```
