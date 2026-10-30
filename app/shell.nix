# Linux development shell for the Marginalia Tauri app (desktop build and tests). From app/:
#   nix-shell --run 'npm ci && npx tauri build --no-bundle' </dev/null
# Rust comes from rustup, which picks up the toolchain pinned in ../rust-toolchain.toml (same as the server).
# Android and iOS builds are done by CI (.github/workflows/app.yml); an Android SDK/NDK is not provided here.
{ pkgs ? import <nixpkgs> { } }:
pkgs.mkShell {
  packages = with pkgs; [
    rustup
    gcc
    cmake
    pkg-config
    nodejs
    openssl
    glib
    gtk3
    cairo
    pango
    gdk-pixbuf
    atk
    librsvg
    libsoup_3
    webkitgtk_4_1
    dbus
    xdg-utils
    xvfb-run
  ];
  shellHook = ''
    export XDG_DATA_DIRS="${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.gtk3}/share/gsettings-schemas/${pkgs.gtk3.name}:$XDG_DATA_DIRS"
    export GIO_MODULE_DIR="${pkgs.glib-networking}/lib/gio/modules"
  '';
}
