#!/bin/sh
# Run as root on 185.198.245.40 with a release binary of jam-collector.
set -eu

if [ "$#" -ne 1 ] || [ ! -f "$1" ]; then
    echo "Usage: sudo sh deploy/collector/install-collector.sh /path/to/release/jam-collector" >&2
    exit 2
fi
if [ "$(id -u)" -ne 0 ]; then
    echo "Run this installer as root." >&2
    exit 1
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
command -v systemctl >/dev/null

if ! getent passwd jamcollector >/dev/null; then
    useradd --system --user-group --no-create-home --shell /bin/false jamcollector
fi
test "$(id -u jamcollector)" -ne 0
test "$(getent passwd jamcollector | cut -d: -f7)" = /bin/false
getent group jamcollector >/dev/null

install -d -o root -g root -m 0755 /usr/local/lib/virtual-jam
install -o root -g root -m 0755 "$1" /usr/local/lib/virtual-jam/jam-collector.new
mv -f /usr/local/lib/virtual-jam/jam-collector.new /usr/local/lib/virtual-jam/jam-collector

install -d -o jamcollector -g jamcollector -m 0750 /var/log/jam-collector
install -d -o root -g jamcollector -m 0750 /etc/virtual-jam
if [ ! -f /etc/virtual-jam/collector.env ]; then
    cat <<'EOF' > /etc/virtual-jam/collector.env
# Optional shared beta token (uncomment to require X-Jam-Beta-Key or Authorization: Bearer)
# JAM_COLLECTOR_BETA_TOKEN=replace-with-strong-beta-token
JAM_COLLECTOR_RATE_LIMIT=30
JAM_COLLECTOR_RETENTION_DAYS=30
JAM_COLLECTOR_MAX_DISK_MB=2048
EOF
    chown root:jamcollector /etc/virtual-jam/collector.env
    chmod 0640 /etc/virtual-jam/collector.env
fi

systemd-analyze verify "$script_dir/jam-collector.service"
install -o root -g root -m 0644 "$script_dir/jam-collector.service" \
    /etc/systemd/system/jam-collector.service

systemctl daemon-reload
systemctl enable jam-collector.service
systemctl restart jam-collector.service
sleep 2
systemctl is-active --quiet jam-collector.service
systemctl --no-pager --full status jam-collector.service
