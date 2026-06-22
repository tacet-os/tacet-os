{
  description = "tacet-os — a quiet, agent-native Wayland desktop";

  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor = forAllSystems (system: nixpkgs.legacyPackages.${system});

      # Compositor build inputs. Kept as a function so it can be reused
      # by both packages.tacet-compositor and devShells.default.
      compositorDeps = pkgs: rec {
        native = with pkgs; [ pkg-config ];
        # Smithay (and wayland-backend) dlopen these at runtime. Cargo
        # doesn't bake an rpath into the binary, so they must be on
        # LD_LIBRARY_PATH (dev) or wrapped in (release).
        runtime = with pkgs; [
          seatd libdisplay-info libinput libxkbcommon
          wayland mesa libGL udev pixman libgbm
        ];
      };
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = pkgsFor.${system};
          deps = compositorDeps pkgs;
        in
        {
          tacet-compositor = pkgs.rustPlatform.buildRustPackage {
            pname = "tacet-compositor";
            version = "0.1.0-alpha.1";

            # Whole workspace is the build source so cargo can resolve
            # workspace members. cargo only builds the compositor crate
            # because of buildAndTestSubdir.
            src = ./.;

            cargoLock = {
              lockFile = ./Cargo.lock;
              # Smithay isn't on crates.io for 0.7.x yet — pinned via
              # the git rev in Cargo.lock. Both hashes are identical
              # because both crates come from the same smithay repo at
              # the same commit. If Cargo.lock's smithay rev moves,
              # `nix build` will fail with the new expected hash; paste
              # it back in here.
              outputHashes = {
                "smithay-0.7.0" = "sha256-hclOFFKWY2hjVEQrE/whFuppf72JuwNoV2UwBk/pAh4=";
                "smithay-drm-extras-0.1.0" = "sha256-hclOFFKWY2hjVEQrE/whFuppf72JuwNoV2UwBk/pAh4=";
              };
            };

            buildAndTestSubdir = "crates/apps/compositor";

            nativeBuildInputs = deps.native ++ [ pkgs.makeWrapper ];
            buildInputs = deps.runtime;

            # winit backend is for nested dev (`cargo run -- --winit`);
            # the installed binary runs on tty-udev for a real session.
            buildNoDefaultFeatures = true;
            buildFeatures = [ "egl" "udev" "xwayland" ];

            postInstall = ''
              wrapProgram $out/bin/tacet-compositor \
                --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath deps.runtime} \
                --prefix PATH : ${pkgs.xwayland}/bin

              mkdir -p $out/share/wayland-sessions
              cat > $out/share/wayland-sessions/tacet.desktop <<EOF
              [Desktop Entry]
              Name=tacet
              Comment=tacet-os Wayland session
              Exec=$out/bin/tacet-compositor --tty-udev
              Type=Application
              DesktopNames=tacet
              EOF
            '';

            meta = with pkgs.lib; {
              description = "Wayland compositor for tacet-os (smithay-based)";
              homepage = "https://github.com/tacet-os/tacet-os";
              license = licenses.mit;
              mainProgram = "tacet-compositor";
              platforms = [ "x86_64-linux" "aarch64-linux" ];
            };

            # NixOS's sessionPackages mechanism requires this — must
            # match the basename of the .desktop file emitted above.
            passthru.providedSessions = [ "tacet" ];
          };

          default = self.packages.${system}.tacet-compositor;
        });

      # Importable NixOS modules.
      #
      #   inputs.tacet-os.url = "github:tacet-os/tacet-os";
      #   imports = [ inputs.tacet-os.nixosModules.default ];
      #
      # - `default` — base defaults + tacet session. The everyday choice.
      # - `base`    — Wayland defaults only (rtkit, dbus, xwayland, seatd,
      #               NIXOS_OZONE_WL). Useful if you don't want tacet as
      #               a session but want the surrounding system hygiene.
      # - `session` — just the session entry. Pair with your own base.
      nixosModules = {
        base = import ./nix/modules/base.nix;
        session = import ./nix/modules/tacet-session.nix self;
        default = { ... }: {
          imports = [
            self.nixosModules.base
            self.nixosModules.session
          ];
        };
      };

      devShells = forAllSystems (system:
        let
          pkgs = pkgsFor.${system};
          deps = compositorDeps pkgs;
        in
        {
          default = pkgs.mkShell {
            nativeBuildInputs = deps.native;
            buildInputs = deps.runtime ++ (with pkgs; [
              # rust toolchain
              rustc cargo rust-analyzer rustfmt clippy
              # ts / js: bun manages the packages/* workspace; node is here
              # because vite + many JS tools shell out to node directly.
              # npm comes bundled with nodejs.
              bun nodejs_22
              # nix tooling — formatter + LSP + pretty build output
              nixpkgs-fmt nil nix-output-monitor
              # vcs — included so the shell is fully self-contained;
              # don't depend on the host's git version
              git
            ]);
            # Required for `cargo run --features udev`: dlopen at runtime.
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath deps.runtime;

            # moonrepo (polyglot task runner) is intentionally NOT a nix
            # package here — current nixpkgs `moon` has a broken transitive
            # rust dep (`sdd` lib won't compile). It lives in the root
            # package.json's devDependencies instead. After `nix develop`,
            # run `bun install` once; then `bun run moon ...` orchestrates
            # tasks across crates/* and packages/*.
            shellHook = ''
              if [ ! -d node_modules ]; then
                echo "tacet-os dev shell: run \`bun install\` to fetch JS deps (incl. moon)"
              fi
            '';
          };
        });

      # `nix flake check` runs these. Wires the package build into CI.
      checks = forAllSystems (system: {
        compositor = self.packages.${system}.tacet-compositor;
      });

      formatter = forAllSystems (system: pkgsFor.${system}.nixpkgs-fmt);
    };
}
