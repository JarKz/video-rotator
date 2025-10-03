{
  description = "A very basic flake";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";

      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [
          rust-overlay.overlays.default
        ];
      };

      rustPackage = pkgs.rust-bin.stable.latest.default.override {
        extensions = [
          "rust-src"
          "rustfmt"
          "clippy"
        ];
      };
    in
    {

      devShells."${system}".default = pkgs.mkShell {
        packages = with pkgs; [
          llvmPackages_20.clang-unwrapped.lib
          llvmPackages_20.libllvm.lib
          kdePackages.full
        ];

        buildInputs = with pkgs; [
          rustPackage
          ffmpeg_8-full
          x264
          nasm
          llvmPackages_20.libcxxClang
          libGL
          lld
          gcc
        ];

        nativeBuildInputs = with pkgs; [
          pkg-config
        ];

        shellHook = ''
          export LD_LIBRARY_PATH=$LD_LIBRARY_PATH:${
            pkgs.lib.makeLibraryPath (
              with pkgs;
              [
                llvmPackages_20.clang-unwrapped.lib
                llvmPackages_20.libllvm.lib
                ffmpeg_8-full
                x264.lib
                kdePackages.full
              ]
            )
          }

          zsh
        '';
      };
    };
}
