#!/bin/sh
# Run as root on the target, with a release binary built for that machine.
set -eu

if [ "$#" -ne 1 ] || [ ! -f "$1" ]; then
    echo "Usage: sudo sh deploy/install.sh /path/to/release/jam-server" >&2
    exit 2
fi
if [ "$(id -u)" -ne 0 ]; then
    echo "Run this installer as root." >&2
    exit 1
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
command -v systemctl >/dev/null
command -v ufw >/dev/null

if ! getent passwd jamserver >/dev/null; then
    useradd --system --user-group --no-create-home --shell /bin/false jamserver
fi
# Refuse to run under an existing interactive or privileged identity.
test "$(id -u jamserver)" -ne 0
test "$(getent passwd jamserver | cut -d: -f7)" = /bin/false
getent group jamserver >/dev/null

install -d -o root -g root -m 0755 /usr/local/lib/virtual-jam
# Rename avoids overwriting a running executable and preserves root ownership.
install -o root -g root -m 0755 "$1" /usr/local/lib/virtual-jam/jam-server.new
mv -f /usr/local/lib/virtual-jam/jam-server.new /usr/local/lib/virtual-jam/jam-server
systemd-analyze verify "$script_dir/jam-server.service"
install -o root -g root -m 0644 "$script_dir/jam-server.service" \
    /etc/systemd/system/jam-server.service
ufw allow 7777/udp
# Do not enable UFW here: doing so could disconnect the administrator.
ufw status
systemctl daemon-reload
systemctl enable jam-server.service
systemctl restart jam-server.service
sleep 3
systemctl is-active --quiet jam-server.service
systemctl --no-pager --full status jam-server.service
