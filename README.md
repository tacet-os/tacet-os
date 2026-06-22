# tacet-os

> *tacet* (Latin): "is silent"

A quiet, agent-native Wayland desktop. Polyglot monorepo: rust compositor, ts launcher, nix integration.

**Status:** v0.1.0-alpha.1 — pre-release. APIs, layout, and behavior will change without notice.

## What's here

| Path | What | Language |
| --- | --- | --- |
| `crates/compositor/` | `tacet-compositor` — smithay-based Wayland compositor | Rust |
| `packages/launcher/` | `tacet-launcher` — app launcher surface (stub) | TypeScript |
| `nix/modules/` | NixOS module — adds tacet as a selectable session | Nix |
| `flake.nix` | The assembly: packages, module, devShell, checks | Nix |
| `moon.yml` + `.moon/` | Polyglot task orchestration | moonrepo |

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

```bash
nix develop                          # rust + node + nix tooling
moon run compositor:build            # build tacet-compositor
moon run launcher:dev                # vite dev server for the launcher
nix build .#tacet-compositor         # build the nix package
nix flake check                      # validate flake + run checks
```

## License

MIT. See [LICENSE](LICENSE). Derived from [smithay/anvil](https://github.com/Smithay/smithay) (MIT).
