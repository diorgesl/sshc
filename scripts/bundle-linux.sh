#!/usr/bin/env bash
# Gera dist/sshc_<versao>_<arch>.deb a partir do build release.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
ARCH=$(dpkg --print-architecture)
PKG="target/deb/sshc_${VERSION}_${ARCH}"
DEB="dist/sshc_${VERSION}_${ARCH}.deb"

echo "== build release =="
cargo build --release -p sshc-gui -p sshc

echo "== pacote =="
rm -rf target/deb
mkdir -p "$PKG/DEBIAN" "$PKG/usr/bin" "$PKG/usr/share/applications"
install -m 755 target/release/sshc-gui "$PKG/usr/bin/sshc-gui"
install -m 755 target/release/sshc "$PKG/usr/bin/sshc"

cat > "$PKG/usr/share/applications/sshc.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=sshc
Comment=Cliente SSH multi-sessão
Exec=/usr/bin/sshc-gui
Icon=utilities-terminal
Terminal=false
Categories=Network;Development;
DESKTOP

SIZE=$(du -sk "$PKG/usr" | cut -f1)
cat > "$PKG/DEBIAN/control" <<CONTROL
Package: sshc
Version: ${VERSION}
Section: net
Priority: optional
Architecture: ${ARCH}
Depends: libc6, libgcc-s1, libudev1, libxkbcommon0, libwayland-client0, libx11-6, libgl1, libegl1, libxcursor1, libxrandr2, libxi6, libxinerama1
Installed-Size: ${SIZE}
Maintainer: diorgesl <diorgesl@gmail.com>
Description: Cliente SSH multi-sessão
 Cliente SSH com abas, sessoes SecureCRT, temas, backup no Telegram
 e atualizacao pelo GitHub.
CONTROL

mkdir -p dist
dpkg-deb --build --root-owner-group "$PKG" "$DEB"
rm -rf target/deb
echo "pronto: $DEB"
