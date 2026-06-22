# Session piece — adds tacet to the greeter's session picker plus the
# pieces a fresh tacet session needs to feel complete (terminal +
# XDG default resolution). Pair with base.nix for the full tacet-os
# experience — `nixosModules.default` does both for you.
self: { config, pkgs, lib, ... }:
{
  # Adds the tacet session to the greeter's selectable sessions.
  # Other sessions (COSMIC/GNOME/sway/etc.) are untouched.
  services.displayManager.sessionPackages = [
    self.packages.${pkgs.system}.tacet-compositor
  ];

  # tacet-browser ships as the default contextless web surface. We
  # intentionally don't set BROWSER, since a persistent-profile
  # browser is a reasonable default for many users — consumers who
  # want tacet-browser system-wide can opt in with
  # `environment.sessionVariables.BROWSER = "tacet-browser"`.
  #
  # TODO(terminal): tacet-terminal package + .desktop + xdg-terminals.list
  # + TACET_TERMINAL session var are commented out until the crate
  # lands on `dev` (see flake.nix). Without them, Super+Enter falls
  # through to $TERMINAL / xdg-terminal-exec / foot/alacritty/kitty
  # auto-discovery, which is fine for now.
  environment.systemPackages = [
    # self.packages.${pkgs.system}.tacet-terminal
    self.packages.${pkgs.system}.tacet-browser
    pkgs.xdg-terminal-exec
  ];

  # environment.etc."xdg/xdg-terminals.list".text = ''
  #   tacet-terminal.desktop
  # '';
  # environment.sessionVariables.TACET_TERMINAL = lib.mkDefault "tacet-terminal";
}
