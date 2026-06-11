{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      # The runtime stack (mutter, pipewire, at-spi2-core, GStreamer) is
      # Linux-only, so we build for the Linux ABIs we care about. Adding a
      # new arch here is all it takes to fan the outputs out to it.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      # Build the full output set once per system, then project each
      # attribute category out below. Keeping it in a single per-system
      # `let` means the shared bindings (gstPluginPath, devPackages,
      # refresh) are defined once per arch rather than repeated per output.
      perSystem = nixpkgs.lib.genAttrs systems (
        system:
        let
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
            # GTK4 + its pkg-config-advertised transitive deps — linked against
            # by the waydriver-fixture-gtk demo crate. `buildEnv` doesn't follow
            # propagated inputs, so every pc dep GTK4 declares has to appear
            # here by name. `out` is needed at runtime; `dev` carries .pc files.
            gtk4
            gtk4.dev
            pango.dev
            cairo.dev
            gdk-pixbuf.dev
            harfbuzz.dev
            libepoxy.dev
            fribidi.dev
            libxkbcommon.dev
            wayland.dev
            vulkan-headers
            vulkan-loader.dev
            # libadwaita — the GNOME HIG widget layer on top of GTK4. The
            # fixture uses Adw widgets alongside raw GTK4 ones so we can
            # isolate AT-SPI behavior to whichever layer actually produces
            # the output for a given test.
            libadwaita
            libadwaita.dev
            appstream.dev
            # CI / release verification tooling
            actionlint
            act
            release-plz
          ];

          refresh = pkgs.writeShellScriptBin "refresh" ''
            nix build .#packages.${system}.dev-profile --out-link .nix-profile
          '';

          # Run the `#[ignore]`d e2e suite (spawns mutter + pipewire + AT-SPI).
          # A *fresh* session bus is required: waydriver connects accessibility
          # to the host session bus, and the host login bus can't activate the
          # nix-store `org.a11y.Bus.service`. `dbus-run-session` gives each run
          # its own bus, which (with the at-spi env the shellHook already sets)
          # activates a11y correctly. `--test-threads=1` because the tests share
          # that bus. Usage: `e2e-tests` (all) or `e2e-tests <name-filter>`.
          e2e-tests = pkgs.writeShellScriptBin "e2e-tests" ''
            set -euo pipefail
            cargo build -p waydriver-fixture-gtk
            exec ${pkgs.dbus}/bin/dbus-run-session -- \
              cargo test -p waydriver-e2e -- --ignored --test-threads=1 "$@"
          '';
        in
        {
          packages = {
            default = pkgs.rustPlatform.buildRustPackage {
              pname = "waydriver";
              version = "0.1.0";
              src = ./.;
              cargoLock.lockFile = ./Cargo.lock;

              # Build just the MCP server — the actual shipped product, and the
              # binary the `mcp` app below wraps. The workspace also contains
              # the GTK4 fixture/examples crates, but those are test scaffolding
              # (built in the dev shell / Docker e2e stages with the full GTK4
              # dev stack); compiling them here would drag in gdk-pixbuf, pango,
              # libadwaita, &c. that the server itself doesn't link.
              cargoBuildFlags = [ "-p" "waydriver-mcp" ];
              cargoTestFlags = [ "-p" "waydriver-mcp" ];

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
              paths = devPackages ++ [ refresh e2e-tests ];
            };
          };

          apps = {
            coverage = {
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

            # nix run .#docker-build — builds the production Docker image
            docker-build = {
              type = "app";
              program =
                let
                  script = pkgs.writeShellScriptBin "waydriver-docker-build" ''
                    exec docker build -t waydriver-mcp "$@" .
                  '';
                in
                "${script}/bin/waydriver-docker-build";
            };

            # nix run .#docker-build-e2e — builds the e2e Docker image (adds the
            # waydriver-fixture-gtk binary and its GTK4/libadwaita runtime libs).
            docker-build-e2e = {
              type = "app";
              program =
                let
                  script = pkgs.writeShellScriptBin "waydriver-docker-build-e2e" ''
                    exec docker build --target runtime-e2e -t waydriver-mcp-e2e "$@" .
                  '';
                in
                "${script}/bin/waydriver-docker-build-e2e";
            };

            # nix run .# — launches the MCP server with runtime deps on PATH
            mcp = {
              type = "app";
              program =
                let
                  wrapper = pkgs.writeShellScriptBin "waydriver-mcp" ''
                    export PATH="${
                      pkgs.lib.makeBinPath [
                        pkgs.dbus
                        pkgs.at-spi2-core
                        pkgs.mutter
                        pkgs.pipewire
                        pkgs.wireplumber
                        pkgs.gst_all_1.gstreamer
                        pkgs.gst_all_1.gst-plugins-base
                        pkgs.gst_all_1.gst-plugins-good
                      ]
                    }:$PATH"
                    # at-spi-bus-launcher lives in libexec
                    export PATH="${pkgs.at-spi2-core}/libexec:$PATH"
                    # D-Bus service files for AT-SPI registry auto-activation
                    export XDG_DATA_DIRS="${pkgs.at-spi2-core}/share:${
                      pkgs.lib.concatStringsSep ":" [
                        "${pkgs.gsettings-desktop-schemas}/share"
                      ]
                    }:''${XDG_DATA_DIRS:-/run/current-system/sw/share}"
                    # GStreamer plugin paths (core, base, good, pipewire)
                    export GST_PLUGIN_PATH="${gstPluginPath}"
                    exec ${self.packages.${system}.default}/bin/waydriver-mcp "$@"
                  '';
                in
                "${wrapper}/bin/waydriver-mcp";
            };
          };

          checks.tests = pkgs.rustPlatform.buildRustPackage {
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

          devShells.default = pkgs.mkShell {
            packages = devPackages ++ [ refresh e2e-tests ];

            shellHook = ''
              refresh
              export PATH="$PWD/.nix-profile/bin:$PATH"
              # at-spi-bus-launcher lives in libexec (not exposed by buildEnv)
              export PATH="${pkgs.at-spi2-core}/libexec:$PATH"
              export XDG_DATA_DIRS="${pkgs.at-spi2-core}/share:${pkgs.gsettings-desktop-schemas}/share:''${XDG_DATA_DIRS:-/run/current-system/sw/share}"
              export GST_PLUGIN_PATH="${gstPluginPath}"
              # pkg-config lookup for packages that only ship their .pc files
              # under the dev-profile (GTK4's `.dev` output is pulled in via
              # devPackages but buildEnv concatenates pkgconfig dirs here):
              export PKG_CONFIG_PATH="$PWD/.nix-profile/lib/pkgconfig:$PWD/.nix-profile/share/pkgconfig:''${PKG_CONFIG_PATH:-}"
              # nixpkgs' rustc doesn't ship the stdlib source tree, so rust-analyzer
              # can't resolve `std`/`core` without this pointer.
              export RUST_SRC_PATH="${pkgs.rustPlatform.rustLibSrc}"
              # Mesa software (llvmpipe) rendering so headless mutter can bring
              # up its Clutter backend without a real GPU. Without this the
              # live-mutter `--ignored` tests fail at startup ("no available
              # drivers found" / "/dev/dri/renderD128" missing) because this
              # environment has no DRI/EGL driver — the reason such tests
              # otherwise only run in the Docker image (Fedora ships Mesa at
              # standard paths). The driver dirs are dlopen'd via these vars;
              # LD_LIBRARY_PATH lets the EGL vendor lib (libEGL_mesa.so.0,
              # referenced relatively by 50_mesa.json) resolve.
              export LIBGL_ALWAYS_SOFTWARE=1
              export GALLIUM_DRIVER=llvmpipe
              export LIBGL_DRIVERS_PATH="${pkgs.mesa}/lib/dri"
              export GBM_BACKENDS_PATH="${pkgs.mesa}/lib/gbm"
              export __EGL_VENDOR_LIBRARY_DIRS="${pkgs.mesa}/share/glvnd/egl_vendor.d"
              export LD_LIBRARY_PATH="${pkgs.mesa}/lib:''${LD_LIBRARY_PATH:-}"
            '';
          };
        }
      );
    in
    {
      packages = nixpkgs.lib.mapAttrs (_: v: v.packages) perSystem;
      apps = nixpkgs.lib.mapAttrs (_: v: v.apps) perSystem;
      checks = nixpkgs.lib.mapAttrs (_: v: v.checks) perSystem;
      devShells = nixpkgs.lib.mapAttrs (_: v: v.devShells) perSystem;
    };
}
