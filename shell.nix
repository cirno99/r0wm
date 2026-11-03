# 开发用 shell：提供构建与运行所需的 C 依赖。
#
# 用法：nix-shell --run 'cargo run --bin r0wm'
# 只做嵌套开发（winit 后端）时实际只需要 libxkbcommon 与 wayland；
# 其余包是 udev 后端（ADR 0002）的依赖，一起给出免得来回切环境。
{ pkgs ? import <nixpkgs> { } }:
let
  inputs = with pkgs; [
    libxkbcommon
    wayland
    mesa
    libGL
    seatd
    libinput
    libdrm
    libgbm
    udev
    xwayland
  ];
in
pkgs.mkShell {
  nativeBuildInputs = with pkgs; [
    pkg-config
  ];

  buildInputs = inputs;

  # winit/EGL 用 dlopen 找库，所以除了链接期还需要运行期路径。
  shellHook = ''
    export LD_LIBRARY_PATH=${pkgs.lib.makeLibraryPath inputs}:$LD_LIBRARY_PATH
  '';
}
