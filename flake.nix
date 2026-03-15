{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };

      gstPluginPath = pkgs.lib.makeSearchPath "lib/gstreamer-1.0" [
        pkgs.gst_all_1.gstreamer.out
        pkgs.gst_all_1.gst-plugins-base
        pkgs.gst_all_1.gst-plugins-good
        pkgs.pipewire
      ];

      devPackages = with pkgs; [
        cargo
        rustc
        rustfmt
        clippy
        rust-analyzer
        pkg-config
        dbus
        at-spi2-core
        mutter
        pipewire
        wireplumber
        gst_all_1.gstreamer
        gst_all_1.gst-plugins-base
        gst_all_1.gst-plugins-good
        gnome-calculator
      ];

      refresh = pkgs.writeShellScriptBin "refresh" ''
        nix build .#packages.${system}.dev-profile --out-link .nix-profile
      '';
    in
    {
      packages.${system} = {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "waydriver";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = with pkgs; [ pkg-config ];
          buildInputs = with pkgs; [
            dbus
            at-spi2-core
            gst_all_1.gstreamer
            gst_all_1.gst-plugins-base
            gst_all_1.gst-plugins-good
            pipewire
          ];
        };

        dev-profile = pkgs.buildEnv {
          name = "waydriver-dev-profile";
          paths = devPackages ++ [ refresh ];
        };

      };

      apps.${system}.coverage = {
        type = "app";
        program =
          let
            script = pkgs.writeShellScriptBin "waydriver-coverage" ''
              export PATH="${
                pkgs.lib.makeBinPath [
                  pkgs.cargo
                  pkgs.rustc
                  pkgs.pkg-config
                  pkgs.cargo-tarpaulin
                  pkgs.dbus
                  pkgs.at-spi2-core
                  pkgs.mutter
                  pkgs.pipewire
                  pkgs.wireplumber
                  pkgs.gst_all_1.gstreamer
                  pkgs.gst_all_1.gst-plugins-base
                  pkgs.gst_all_1.gst-plugins-good
                  pkgs.gnome-calculator
                ]
              }:$PATH"
              export PATH="${pkgs.at-spi2-core}/libexec:$PATH"
              export XDG_DATA_DIRS="${pkgs.at-spi2-core}/share:${pkgs.gsettings-desktop-schemas}/share:''${XDG_DATA_DIRS:-/run/current-system/sw/share}"
              export GST_PLUGIN_PATH="${gstPluginPath}"
              exec cargo tarpaulin --workspace --skip-clean --out stdout "$@"
            '';
          in
          "${script}/bin/waydriver-coverage";
      };

      checks.${system}.tests = pkgs.rustPlatform.buildRustPackage {
        pname = "waydriver-tests";
        version = "0.1.0";
        src = ./.;
        cargoLock.lockFile = ./Cargo.lock;

        nativeBuildInputs = with pkgs; [ pkg-config ];
        buildInputs = with pkgs; [
          dbus
          at-spi2-core
          gst_all_1.gstreamer
          gst_all_1.gst-plugins-base
          gst_all_1.gst-plugins-good
          pipewire
        ];

        nativeCheckInputs = with pkgs; [
          dbus
          at-spi2-core
          mutter
          pipewire
          wireplumber
          gst_all_1.gstreamer
          gst_all_1.gst-plugins-base
          gst_all_1.gst-plugins-good
          gnome-calculator
        ];

        checkPhase = ''
          export HOME=$(mktemp -d)
          export XDG_RUNTIME_DIR=$(mktemp -d)
          export PATH="${pkgs.at-spi2-core}/libexec:$PATH"
          export XDG_DATA_DIRS="${pkgs.at-spi2-core}/share:${pkgs.gsettings-desktop-schemas}/share:''${XDG_DATA_DIRS:-}"
          export GST_PLUGIN_PATH="${gstPluginPath}"
          cargo test --workspace
        '';
      };

      devShells.${system}.default = pkgs.mkShell {
        packages = devPackages ++ [ refresh ];

        shellHook = ''
          refresh
          export PATH="$PWD/.nix-profile/bin:$PATH"
          # at-spi-bus-launcher lives in libexec (not exposed by buildEnv)
          export PATH="${pkgs.at-spi2-core}/libexec:$PATH"
          export XDG_DATA_DIRS="${pkgs.at-spi2-core}/share:${pkgs.gsettings-desktop-schemas}/share:''${XDG_DATA_DIRS:-/run/current-system/sw/share}"
          export GST_PLUGIN_PATH="${gstPluginPath}"
        '';
      };
    };
}
