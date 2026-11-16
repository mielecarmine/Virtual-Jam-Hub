#!/usr/bin/env bash
set -euo pipefail
version=${1:?version required}
arch=${2:?architecture required}
app='target/package-macos/Virtual Jam.app'
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" dist
install -m755 target/release/jam-client "$app/Contents/MacOS/jam-client"
cp LICENSE "$app/Contents/Resources/LICENSE"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>io.github.mielecarmine.virtual-jam</string>
<key>CFBundleName</key><string>Virtual Jam</string>
<key>CFBundleExecutable</key><string>jam-client</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>1.0.0</string>
<key>CFBundleVersion</key><string>1.0.0</string>
<key>LSMinimumSystemVersion</key><string>11.0</string>
<key>NSHighResolutionCapable</key><true/>
<key>NSMicrophoneUsageDescription</key><string>Virtual Jam uses audio input to share your instrument and voice with your session.</string>
</dict></plist>
PLIST
plutil -lint "$app/Contents/Info.plist"
codesign --force --deep --sign - "$app"
codesign --verify --deep --strict "$app"
pkgbuild --component "$app" --install-location /Applications --identifier io.github.mielecarmine.virtual-jam --version 1.0.0 "dist/Virtual-Jam-${version}-macos-${arch}.pkg"
sudo installer -pkg "dist/Virtual-Jam-${version}-macos-${arch}.pkg" -target /
'/Applications/Virtual Jam.app/Contents/MacOS/jam-client' --help
