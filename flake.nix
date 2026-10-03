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

          # The ocrs models behind the `visual` locator, fetched at build time
          # so the server never downloads them on first use. The hashes are
          # the ones `crates/waydriver/src/visual/models.rs` checks downloads
          # against; bump both together.
          ocrsModels = {
            detection = pkgs.fetchurl {
              url = "https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten";
              sha256 = "f15cfb56bd02c4bf478a20343986504a1f01e1665c2b3a0ad66340f054b1b5ca";
            };
            recognition = pkgs.fetchurl {
              url = "https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.rten";
              sha256 = "e484866d4cce403175bd8d00b128feb08ab42e208de30e42cd9889d8f1735a6e";
            };
          };

          # The private session bus the `mcp` wrapper runs the server on. Its
          # only activatable services are at-spi2-core's. With the system's
          # session.conf instead, the bus also activates the host's
          # xdg-desktop-portal, which on a bus with no desktop session behind
          # it never answers. GDK queries the portal synchronously at
          # startup, so every app then stalls for D-Bus's 25s timeout and
          # misses the 10s AT-SPI registry deadline.
          sessionBusConfig = pkgs.writeText "waydriver-session.conf" ''
            <!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
             "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
            <busconfig>
              <type>session</type>
              <keep_umask/>
              <listen>unix:tmpdir=/tmp</listen>
              <auth>EXTERNAL</auth>
              <servicedir>${pkgs.at-spi2-core}/share/dbus-1/services</servicedir>
              <policy context="default">
                <allow send_destination="*" eavesdrop="true"/>
                <allow eavesdrop="true"/>
                <allow own="*"/>
              </policy>
            </busconfig>
          '';

          # The MCP server with its runtime deps injected. A package, not
          # just an app, so a NixOS / home-manager config can put it in an
          # `mcpServers` entry (`lib.getExe waydriver.packages.<system>.mcp`).
          #
          # It runs on a fresh session bus, as the Docker entrypoint and
          # `e2e-tests` do, since the host login bus can't activate the
          # nix-store `org.a11y.Bus.service` (and a container may have no
          # session bus at all). Three things make that work:
          #
          # - ATSPI_DBUS_IMPLEMENTATION=dbus-daemon: nixpkgs builds
          #   at-spi-bus-launcher to try dbus-broker first, and
          #   dbus-broker-launch asks systemd on the session bus to start it,
          #   which a private bus has no systemd to answer.
          # - sessionBusConfig above, for the portal.
          # - stdout: the bus and every service it activates inherit the
          #   wrapper's stdout, which is the MCP stdio transport, and
          #   at-spi2-registryd prints a banner there in the middle of the
          #   JSON-RPC stream. The bus gets stderr; only the server gets the
          #   real stdout back.
          mcp = pkgs.writeShellScriptBin "waydriver-mcp" ''
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
            export XDG_DATA_DIRS="${pkgs.at-spi2-core}/share:${pkgs.gsettings-desktop-schemas}/share''${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}"
            # GStreamer plugin paths (core, base, good, pipewire)
            export GST_PLUGIN_PATH="${gstPluginPath}"
            export ATSPI_DBUS_IMPLEMENTATION=dbus-daemon
            export WAYDRIVER_OCRS_DETECTION_MODEL="''${WAYDRIVER_OCRS_DETECTION_MODEL:-${ocrsModels.detection}}"
            export WAYDRIVER_OCRS_RECOGNITION_MODEL="''${WAYDRIVER_OCRS_RECOGNITION_MODEL:-${ocrsModels.recognition}}"
            exec 3>&1 1>&2
            exec ${pkgs.dbus}/bin/dbus-run-session --config-file=${sessionBusConfig} -- \
              ${pkgs.bash}/bin/bash -c 'exec "$0" "$@" 1>&3 3>&-' \
              ${self.packages.${system}.default}/bin/waydriver-mcp "$@"
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

            inherit mcp;

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

            # nix run .#mcp — launches the MCP server with runtime deps
            # injected (see `mcp` above)
            mcp = {
              type = "app";
              program = "${mcp}/bin/waydriver-mcp";
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
