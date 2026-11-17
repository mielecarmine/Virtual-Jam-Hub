#!/usr/bin/env bash
set -euo pipefail
version=${1:?version required}
root=target/package-linux
mkdir -p "$root"/{DEBIAN,usr/bin,usr/share/applications,usr/share/doc/virtual-jam} dist
install -m755 target/release/jam-client "$root/usr/bin/virtual-jam"
install -m644 LICENSE "$root/usr/share/doc/virtual-jam/copyright"
cat > "$root/DEBIAN/control" <<CONTROL
Package: virtual-jam
Version: ${version/-/\~}
Section: sound
Priority: optional
Architecture: amd64
Maintainer: Carmine Miele <mielecarmine@users.noreply.github.com>
Depends: libasound2, libc6 (>= 2.35), libgcc-s1, libstdc++6, libxkbcommon0, libwayland-client0, libx11-6, libxcursor1, libxi6, libxrandr2, libgl1
Description: Virtual Jam real-time network audio client
 Low-latency UDP audio sessions and a per-peer desktop mixer.
CONTROL
cat > "$root/usr/share/applications/virtual-jam.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Virtual Jam
Comment=Real-time network audio
Exec=virtual-jam
Terminal=false
Categories=AudioVideo;Audio;
DESKTOP
dpkg-deb --root-owner-group --build "$root" "dist/Virtual-Jam-${version}-linux-amd64.deb"
sudo dpkg -i "dist/Virtual-Jam-${version}-linux-amd64.deb"
virtual-jam --help
