{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  buildInputs = with pkgs; [
    rustup
    mold
    pkg-config
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
    gcc
  ];

  LD_LIBRARY_PATH = with pkgs; lib.makeLibraryPath [
    libxkbcommon
    wayland
    mesa
    libGL
    libdrm
    libgbm
    libdisplay-info
    seatd
    udev
    libinput
  ];
}
