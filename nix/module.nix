# NixOS module for the midilogd capture daemon, mirroring the homelab otelcol
# unit (hosts/n150/modules/otelcol.nix): a dedicated system user,
# StateDirectory, and Restart=on-failure.
#
# Proposal only — apply on the homelab side by adding this to the host's
# `imports` and supplying the package as `pkgs.midilogd`, e.g. via a flake input
# for this repo plus an overlay:
#
#   nixpkgs.overlays = [ (final: prev: {
#     midilogd = final.rustPlatform.buildRustPackage {
#       pname = "midilogd";
#       version = "0.1.0";
#       src = inputs.midilab;              # this repository
#       cargoLock.lockFile = "${inputs.midilab}/Cargo.lock";
#       buildAndTestSubdir = "crates/midilogd";
#       nativeBuildInputs = [ final.pkg-config ];
#       buildInputs = [ final.alsa-lib ];  # alsa-sys links libasound
#     };
#   }) ];

{ pkgs, ... }:

let
  source = "Roland";
  captureDir = "/var/lib/midilogd/capture";
in
{
  users.groups.midilogd = { };
  users.users.midilogd = {
    isSystemUser = true;
    group = "midilogd";
  };

  systemd.services.midilogd = {
    description = "Always-on ALSA sequencer capture (piano -> append-only JSONL)";
    wantedBy = [ "multi-user.target" ];
    serviceConfig = {
      User = "midilogd";
      Group = "midilogd";
      # Read access to the ALSA sequencer / USB-MIDI device.
      SupplementaryGroups = [ "audio" ];
      ExecStart = "${pkgs.midilogd}/bin/midilogd --source ${source} --capture-dir ${captureDir}";
      # snd-seq or the piano may not be ready at boot; the daemon exits and is
      # restarted until the source appears (it also auto-resubscribes at runtime).
      Restart = "on-failure";
      RestartSec = "5s";
      MemoryMax = "128M";
      StateDirectory = "midilogd";
      StateDirectoryMode = "0750";
    };
  };
}
