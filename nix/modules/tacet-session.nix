# NixOS module body. Imported by flake.nix as `nixosModules.default`
# with the flake's `self` partially-applied so the module can reach
# `self.packages.${pkgs.system}.tacet-compositor` without re-importing.
self: { config, pkgs, lib, ... }:
{
  # Adds the tacet session to the greeter's selectable sessions.
  # Other sessions (COSMIC/GNOME/sway/etc.) are untouched.
  services.displayManager.sessionPackages = [
    self.packages.${pkgs.system}.tacet-compositor
  ];

  # seatd is required for libseat / DRM device access from the
  # compositor's session. Most desktops already enable it; ensure it's on.
  services.seatd.enable = lib.mkDefault true;
}
