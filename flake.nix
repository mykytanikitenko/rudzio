{
  description = "";

  inputs = {
    # Pinned nixpkgs (2026-04-14) - locked via flake.lock
    # Includes: fish, starship, atuin, sccache, just, taplo, nixfmt, jq, podman
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    # Opencode with pre-built binaries (auto-updating)
    opencode-flake = {
      url = "github:dan-online/opencode-nix";
    };
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      opencode-flake,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          config.allowUnsupportedSystem = true;
        };
        opencode-pkg = opencode-flake.packages.${system}.opencode;
        isDarwin = pkgs.stdenv.isDarwin;
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = [
            # Rust toolchain (managed by rustup, pinned via rust-toolchain.toml)
            pkgs.rustup

            # Task runner
            pkgs.just

            # Formatters
            pkgs.taplo # TOML
            pkgs.nixfmt # Nix

            # Rust build cache
            pkgs.sccache

            # Shell environment (fish 4.6.0, starship 1.24.2, atuin 18.13.6)
            pkgs.fish
            pkgs.starship
            pkgs.atuin

            # Opencode (pre-built binaries from dan-online/opencode-nix)
            opencode-pkg

            # Additional tools
            pkgs.jq
            pkgs.podman
            pkgs.git
          ];

          shellHook = ''
            export IN_NIX_SHELL=1
            export RUST_BACKTRACE=full

            # Use sccache for cargo builds
            export RUSTC_WRAPPER="${pkgs.sccache}/bin/sccache"
            export SCCACHE_CACHE_DIR="$PWD/.cache/sccache-nix"
            export SCCACHE_DIRECT=true
            export SCCACHE_COMPRESS=true

            # Isolated opencode configuration (use nix-specific config)
            export OPENCODE_CONFIG_DIR="$PWD/.config/opencode"
            export OPENCODE_CONFIG="$PWD/.config/opencode/opencode.nix.json"

            # Isolated fish shell configuration (prevent using global ~/.config/fish)
            export XDG_CONFIG_HOME="$PWD/.config"
            export XDG_CACHE_HOME="$PWD/.cache/fish-nix"
            export XDG_DATA_HOME="$PWD/.local/fish-nix"

            # Isolated starship configuration
            export STARSHIP_CONFIG="$PWD/.config/starship-nix.toml"

            # Isolated atuin configuration
            export ATUIN_CONFIG_DIR="$PWD/.config/atuin-nix"
            export ATUIN_DB_DIR="$PWD/.data/atuin"

            # Podman/Docker socket for host access (macOS only)
            ${if isDarwin then ''export DOCKER_HOST="unix://$HOME/.local/share/containers/podman/machine/podman.sock"'' else ""}

            # Alias just to use Justfile.nix inside nix shell
            alias just='just -f Justfile.nix'

            # Start fish shell as default (if not already in fish)
            if [ "$SHELL" != "${pkgs.fish}/bin/fish" ]
            then
              exec ${pkgs.fish}/bin/fish --login
            fi

            echo "  just build   — cargo build"
            echo "  just test    — cargo test"
            echo "  just check   — fmt + clippy + test"
            echo "  just fmt     — format all (Rust, TOML, Nix)"
          '';
        };
      }
    );
}
