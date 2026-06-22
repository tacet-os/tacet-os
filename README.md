# tacet-os

> *tacet* (Latin): "is silent"

A quiet, agent-native Wayland desktop. Polyglot monorepo: rust compositor, ts launcher, nix integration.

**Status:** v0.1.0-alpha.1 — pre-release. APIs, layout, and behavior will change without notice.

## What's here

| Path | What | Language |
| --- | --- | --- |
| `crates/apps/compositor/` | `tacet-compositor` — smithay-based Wayland compositor | Rust |
| `crates/libs/` | shared Rust libraries (empty for alpha.1) | Rust |
| `packages/apps/launcher/` | `tacet-launcher` — app launcher surface (stub) | TypeScript |
| `packages/libs/` | shared TS code (empty for alpha.1) | TypeScript |
| `nix/modules/` | NixOS module — adds tacet as a selectable session | Nix |
| `flake.nix` | The assembly: packages, module, devShell, checks | Nix |
| `moon.yml` + `.moon/` | Polyglot task orchestration | moonrepo |

**Rule of thumb:** `apps/*` = things users install or run; `libs/*` = internal building blocks other code depends on. The directory tells you the role without opening the manifest.

## Install (NixOS, flake user)

```nix
# /etc/nixos/flake.nix
{
  inputs.tacet-os.url = "github:tacet-os/tacet-os/v0.1.0-alpha.1";

  outputs = { self, nixpkgs, tacet-os, ... }: {
    nixosConfigurations.your-host = nixpkgs.lib.nixosSystem {
      modules = [
        tacet-os.nixosModules.default
        # ...
      ];
    };
  };
}
```

After `sudo nixos-rebuild switch`, "tacet" appears as a selectable session in your greeter alongside whatever else you have (COSMIC, GNOME, sway, etc.). The module is purely additive — it does not disable or replace anything.

## Develop

The flake's `devShells.default` provides every language toolchain and
system tool needed for this repo — entering it gives you `rustc`, `cargo`,
`rust-analyzer`, `rustfmt`, `clippy`, `bun`, `node`, `npm`, `git`,
`nixpkgs-fmt`, `nil`, plus all Wayland client libs (`libwayland`,
`libxkbcommon`, `libgbm`, etc.) on `LD_LIBRARY_PATH`.

```bash
nix develop                          # enter dev shell (one-time per terminal)
bun install                          # fetch JS deps incl. moonrepo (one-time per checkout)
bun run moon compositor:build        # build tacet-compositor
bun run moon launcher:dev            # vite dev server for the launcher
nix build .#tacet-compositor         # build the nix package (what NixOS consumes)
nix flake check                      # validate flake + run checks
```

`moonrepo` ships via `package.json` devDependencies rather than as a nix
package because current nixpkgs' `moon` derivation has a broken transitive
cargo dep. Using `bun install` to fetch it keeps the dev loop working.

## License

MIT. See [LICENSE](LICENSE). Derived from [smithay/anvil](https://github.com/Smithay/smithay) (MIT).
