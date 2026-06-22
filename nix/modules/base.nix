# tacet-os base — opinionated NixOS defaults that any tacet host
# wants. Everything uses `lib.mkDefault` so a consumer's /etc/nixos
# can override individual settings without `lib.mkForce` gymnastics.
#
# What does NOT belong here: hardware drivers, user accounts, the
# user's app stack (LLMs, containers), networking, firewall rules.
# Those are machine/policy choices and stay in the consumer's config.
{ config, pkgs, lib, ... }:
{
  # Wayland-native rendering for Electron / Chromium / VS Code / Zed
  # etc. Without this they ship XWayland builds with poor HiDPI + IME.
  environment.sessionVariables.NIXOS_OZONE_WL = lib.mkDefault "1";

  # PipeWire / portal init both reach for rtkit on startup. Enabling
  # here means consumers don't have to remember to flip it.
  security.rtkit.enable = lib.mkDefault true;

  # NixOS defaults to dbus-broker. Its peer resource accounting is
  # stricter than the reference dbus-daemon and routinely severs
  # xdg-desktop-portal's glib connections mid-startup ("Peer is being
  # disconnected as it does not have the resources to receive a
  # reply"), which kills the portal frontend — Super+Shift+S and
  # cosmic-screenshot stop working silently. Reference dbus-daemon
  # is a known-good workaround until the broker/glib mismatch is
  # fixed upstream.
  services.dbus.implementation = lib.mkDefault "dbus";

  # X11 legacy app support — companion to the compositor's xwayland
  # feature. Without it, every X11 app fails to launch.
  programs.xwayland.enable = lib.mkDefault true;

  # libseat / DRM device access from the compositor's session. Most
  # desktops (COSMIC, GNOME) already enable it, but ensure it's on.
  services.seatd.enable = lib.mkDefault true;
}
