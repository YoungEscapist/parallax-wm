{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  # Крейты pipewire/libspa (src/portal_stream.rs) генерируют биндинги через
  # bindgen — ему нужен clang и LIBCLANG_PATH. bindgenHook выставляет их сам.
  nativeBuildInputs = with pkgs; [ rustPlatform.bindgenHook ];

  buildInputs = with pkgs; [
    rustup
    mold
    pkg-config
    pipewire
    libxkbcommon
    wayland
    mesa
    libGL
    libinput
    udev
    seatd
    libdrm
    libgbm
    xwayland
    libdisplay-info
    # smithay линкует renderer через -lpixman-1; без пакета mold падает на
    # «library not found: pixman-1» уже после успешной компиляции всех крейтов.
    pixman
    gcc
  ];

  LD_LIBRARY_PATH = with pkgs; lib.makeLibraryPath [
    libxkbcommon
    pipewire
    wayland
    mesa
    libGL
    libdrm
    libgbm
    libdisplay-info
    seatd
    udev
    libinput
    pixman
  ];
}
