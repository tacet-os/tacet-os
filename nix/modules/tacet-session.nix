# Session-only piece. Just adds tacet to the greeter's session
# picker. Pair with base.nix for the full tacet-os experience —
# `nixosModules.default` does both for you.
self: { config, pkgs, lib, ... }:
{
  # Adds the tacet session to the greeter's selectable sessions.
  # Other sessions (COSMIC/GNOME/sway/etc.) are untouched.
  services.displayManager.sessionPackages = [
    self.packages.${pkgs.system}.tacet-compositor
  ];
}
