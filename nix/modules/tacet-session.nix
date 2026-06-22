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

  # tacet-terminal binary + .desktop entry, and the XDG resolver so
  # apps that ask for "the terminal" (Helix :term, file managers, IDEs)
  # find it. tacet-browser ships alongside as the default contextless
  # web surface; we intentionally don't set BROWSER, since a
  # persistent-profile browser is a reasonable default for many users —
  # consumers who want tacet-browser system-wide can opt in with
  # `environment.sessionVariables.BROWSER = "tacet-browser"`.
  environment.systemPackages = [
    self.packages.${pkgs.system}.tacet-terminal
    self.packages.${pkgs.system}.tacet-browser
    pkgs.xdg-terminal-exec
  ];

  # System-wide xdg-terminals.list — first entry wins. Per-user
  # ~/.config/xdg-terminals.list can override if a user wants a
  # different default in their tacet session.
  environment.etc."xdg/xdg-terminals.list".text = ''
    tacet-terminal.desktop
  '';

  # Compositor's Super+Enter resolution chain checks TACET_TERMINAL
  # first; setting it here makes the binding deterministic regardless
  # of $TERMINAL or xdg-terminal-exec discovery order.
  environment.sessionVariables.TACET_TERMINAL = lib.mkDefault "tacet-terminal";
}
