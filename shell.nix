# NixOS build environment.
# - rustup + rust-toolchain.toml: official Rust with its bundled LLVM. The nixpkgs rustc (built against system LLVM 21)
#   miscompiles std::arch AVX-512 VNNI intrinsics used by SurrealDB's vector index (diskann).
# - gcc/cmake/pkg-config: C toolchain for build scripts.
{ pkgs ? import <nixpkgs> { } }:
pkgs.mkShell {
  packages = with pkgs; [ rustup gcc cmake pkg-config ];
}
