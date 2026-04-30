{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.matrix-embed;

  netnsName = "matrix-embed";
  netnsPath = "/run/netns/${netnsName}";

  vethHost = "mxe-host";
  vethNs = "mxe-ns";
  hostAddr4 = "10.91.0.1";
  nsAddr4 = "10.91.0.2";
  prefix4 = 30;

  socksHost = "127.0.0.1";
  socksPort = 25344;

  finalProxy =
    if cfg.wireproxy.enable then "socks5h://${socksHost}:${toString socksPort}" else cfg.proxy;

  matrixEmbedArgs = lib.escapeShellArgs (
    [
      "--homeserver-url"
      cfg.homeserverUrl
    ]
    ++ lib.optionals (cfg.username != null) [
      "--username"
      cfg.username
    ]
    ++ lib.optionals (cfg.passwordFile != null) [
      "--password-file"
      "%d/password"
    ]
    ++ lib.optionals (cfg.recoveryPassphraseFile != null) [
      "--recovery-passphrase-file"
      "%d/recovery-passphrase"
    ]
    ++ lib.concatMap (u: [
      "--trusted-users"
      u
    ]) cfg.trustedUsers
    ++ lib.optionals (cfg.avatarFile != null) [
      "--avatar-file"
      (toString cfg.avatarFile)
    ]
    ++ [
      "--state-store-path"
      cfg.stateStorePath
    ]
    ++ [
      "--database-path"
      cfg.databasePath
    ]
    ++ [
      "--media-store-path"
      cfg.mediaStorePath
    ]
    ++ lib.optionals (cfg.displayName != null) [
      "--display-name"
      cfg.displayName
    ]
    ++ [
      "--command-prefix"
      cfg.commandPrefix
    ]
    ++ lib.optionals (finalProxy != null) [
      "--proxy"
      finalProxy
    ]
  );

  netnsResolvConf = pkgs.writeText "matrix-embed-resolv.conf" (
    lib.concatMapStringsSep "\n" (s: "nameserver ${s}") cfg.nameservers + "\n"
  );

  netnsUp = pkgs.writeShellScript "matrix-embed-netns-up" ''
    set -eu
    PATH=${
      lib.makeBinPath [
        pkgs.iproute2
        pkgs.iptables
        pkgs.coreutils
      ]
    }:$PATH

    ip netns del ${netnsName} 2>/dev/null || true
    ip link del ${vethHost} 2>/dev/null || true

    ip netns add ${netnsName}
    ip -n ${netnsName} link set lo up

    ip link add ${vethHost} type veth peer name ${vethNs}
    ip link set ${vethNs} netns ${netnsName}

    ip addr add ${hostAddr4}/${toString prefix4} dev ${vethHost}
    ip link set ${vethHost} up

    ip -n ${netnsName} addr add ${nsAddr4}/${toString prefix4} dev ${vethNs}
    ip -n ${netnsName} link set ${vethNs} up
    ip -n ${netnsName} route add default via ${hostAddr4}

    iptables -t nat -C POSTROUTING -s ${nsAddr4}/32 ! -o ${vethHost} -j MASQUERADE 2>/dev/null \
      || iptables -t nat -A POSTROUTING -s ${nsAddr4}/32 ! -o ${vethHost} -j MASQUERADE
  '';

  netnsDown = pkgs.writeShellScript "matrix-embed-netns-down" ''
    PATH=${
      lib.makeBinPath [
        pkgs.iproute2
        pkgs.iptables
        pkgs.coreutils
      ]
    }:$PATH
    iptables -t nat -D POSTROUTING -s ${nsAddr4}/32 ! -o ${vethHost} -j MASQUERADE 2>/dev/null || true
    ip link del ${vethHost} 2>/dev/null || true
    ip netns del ${netnsName} 2>/dev/null || true
  '';

  wireproxyConfigBuilder = pkgs.writeShellScript "matrix-embed-wireproxy-config" ''
    set -eu
    umask 0077
    : "''${CREDENTIALS_DIRECTORY:?LoadCredential not set up}"
    : "''${RUNTIME_DIRECTORY:?RuntimeDirectory not set up}"

    private_key="$(cat "$CREDENTIALS_DIRECTORY/privatekey")"
    address="$(cat "$CREDENTIALS_DIRECTORY/address")"
    peerendpoint="$(cat "$CREDENTIALS_DIRECTORY/peerendpoint")"
    peerpublickey="$(cat "$CREDENTIALS_DIRECTORY/peerpublickey")"
    ${lib.optionalString (cfg.wireproxy.presharedKeyFile != null) ''
      preshared_key="$(cat "$CREDENTIALS_DIRECTORY/presharedkey")"
    ''}

    cat > "$RUNTIME_DIRECTORY/wireproxy.conf" <<NIXEOF
    [Interface]
    PrivateKey = $private_key
    Address = $address
    ${lib.optionalString (
      cfg.wireproxy.dns != [ ]
    ) "DNS = ${lib.concatStringsSep ", " cfg.wireproxy.dns}"}
    ${lib.optionalString (cfg.wireproxy.mtu != null) "MTU = ${toString cfg.wireproxy.mtu}"}

    [Peer]
    PublicKey = $peerpublickey
    Endpoint = $peerendpoint
    AllowedIPs = ${lib.concatStringsSep ", " cfg.wireproxy.peer.allowedIPs}
    ${lib.optionalString (
      cfg.wireproxy.peer.persistentKeepalive != null
    ) "PersistentKeepalive = ${toString cfg.wireproxy.peer.persistentKeepalive}"}
    ${lib.optionalString (cfg.wireproxy.presharedKeyFile != null) "PresharedKey = $preshared_key"}

    [Socks5]
    BindAddress = ${socksHost}:${toString socksPort}
    NIXEOF
  '';

  hardening = {
    ProtectSystem = "strict";
    ProtectHome = true;
    PrivateDevices = true;
    PrivateTmp = true;
    ProtectKernelTunables = true;
    ProtectKernelModules = true;
    ProtectControlGroups = true;
    ProtectClock = true;
    ProtectHostname = true;
    LockPersonality = true;
    RestrictRealtime = true;
    RestrictSUIDSGID = true;
    NoNewPrivileges = true;
    SystemCallArchitectures = "native";
  };
in
{
  options.services.matrix-embed = with lib; {
    enable = mkEnableOption "matrix-embed bot";

    package = mkOption {
      type = types.package;
      description = "matrix-embed package to use.";
    };

    homeserverUrl = mkOption {
      type = types.str;
      example = "https://matrix.example.com";
      description = "Matrix homeserver URL.";
    };

    username = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = ''
        Matrix username for fresh login. Only used when no session is
        persisted yet; once a session exists, the saved access token is
        used.
      '';
    };

    passwordFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = ''
        Path to a file containing the Matrix password. Loaded at unit
        start under root via systemd `LoadCredential=`, so the file may
        be root-only (mode 0400 root:root, e.g. the sops-nix default).
      '';
    };

    recoveryPassphraseFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = ''
        Path to a file containing the cross-signing recovery passphrase.
        Same secret-handling semantics as `passwordFile`.
      '';
    };

    trustedUsers = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [
        "@alice:example.com"
        "@bob:example.com"
      ];
      description = "Matrix user IDs allowed to invite the bot to rooms.";
    };

    avatarFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "Path to an avatar image to upload if the bot has none set.";
    };

    stateStorePath = mkOption {
      type = types.str;
      default = "/var/lib/matrix-embed/state";
      description = "Path to the matrix-sdk SQLite state store directory.";
    };

    databasePath = mkOption {
      type = types.str;
      default = "/var/lib/matrix-embed/matrix-embed.db";
      description = "Path to the bot's persistent SQLite database.";
    };

    mediaStorePath = mkOption {
      type = types.str;
      default = "/var/lib/matrix-embed/media";
      description = "Path to the content-addressable media store directory.";
    };

    displayName = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Display name to set on the bot's profile.";
    };

    commandPrefix = mkOption {
      type = types.str;
      default = "!embedbot";
      description = "Command prefix the bot responds to.";
    };

    nameservers = mkOption {
      type = types.listOf types.str;
      default = [
        "1.1.1.1"
        "9.9.9.9"
      ];
      description = ''
        DNS servers used inside the bot's network namespace. Used for
        homeserver name resolution and (when wireproxy is disabled) for
        third-party requests. Wireproxy SOCKS5 hostname requests are
        resolved via `wireproxy.dns`, not these.
      '';
    };

    proxy = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "socks5h://192.168.1.10:1080";
      description = ''
        URL of an external SOCKS5 / HTTP proxy for third-party requests.
        Mutually exclusive with `services.matrix-embed.wireproxy.enable`;
        when wireproxy is enabled, the proxy URL is set automatically to
        the companion instance.
      '';
    };

    wireproxy = {
      enable = mkEnableOption "companion wireproxy instance providing a SOCKS5 endpoint over WireGuard";

      package = mkOption {
        type = types.package;
        default = pkgs.wireproxy;
        defaultText = literalExpression "pkgs.wireproxy";
        description = "wireproxy package to use.";
      };

      privateKeyFile = mkOption {
        type = types.path;
        description = ''
          Path to a file containing the WireGuard private key. Loaded at
          unit start under root via systemd `LoadCredential=`.
        '';
      };

      presharedKeyFile = mkOption {
        type = types.nullOr types.path;
        default = null;
        description = "Path to a file containing the optional WireGuard preshared key.";
      };

      addressFile = mkOption {
        type = types.path;
        description = "File containing address(es) assigned to the WireGuard interface.";
      };

      dns = mkOption {
        type = types.listOf types.str;
        default = [ "1.1.1.1" ];
        example = [ "10.0.0.1" ];
        description = ''
          DNS server(s) used by wireproxy to resolve hostnames inside the
          tunnel (i.e. for SOCKS5 hostname requests). Without at least
          one entry, hostname-based connections through wireproxy will
          fail.
        '';
      };

      mtu = mkOption {
        type = types.nullOr types.int;
        default = null;
        description = "Optional MTU for the WireGuard interface.";
      };

      peer = {
        publicKeyFile = mkOption {
          type = types.path;
          description = "Path containing WireGuard peer public key.";
        };

        endpointFile = mkOption {
          type = types.path;
          description = "Path containing WireGuard peer endpoint (host:port).";
        };

        allowedIPs = mkOption {
          type = types.listOf types.str;
          default = [
            "0.0.0.0/0"
            "::/0"
          ];
          description = "WireGuard peer AllowedIPs.";
        };

        persistentKeepalive = mkOption {
          type = types.nullOr types.int;
          default = null;
          description = "Optional persistent keepalive interval in seconds.";
        };
      };
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = !(cfg.proxy != null && cfg.wireproxy.enable);
        message = "services.matrix-embed.proxy and services.matrix-embed.wireproxy.enable are mutually exclusive.";
      }
    ];

    boot.kernel.sysctl."net.ipv4.ip_forward" = lib.mkDefault 1;

    systemd.services."matrix-embed-netns" = {
      description = "matrix-embed network namespace";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = "${netnsUp}";
        ExecStop = "${netnsDown}";
      };
    };

    systemd.services."matrix-embed" = {
      description = "matrix-embed bot";
      wantedBy = [ "multi-user.target" ];
      after = [
        "matrix-embed-netns.service"
      ]
      ++ lib.optional cfg.wireproxy.enable "matrix-embed-wireproxy.service";
      requires = [ "matrix-embed-netns.service" ];
      bindsTo = [ "matrix-embed-netns.service" ];
      wants = lib.optional cfg.wireproxy.enable "matrix-embed-wireproxy.service";

      serviceConfig = hardening // {
        Type = "simple";
        DynamicUser = true;
        StateDirectory = "matrix-embed";
        StateDirectoryMode = "0700";

        NetworkNamespacePath = netnsPath;
        BindReadOnlyPaths = [ "${netnsResolvConf}:/etc/resolv.conf" ];

        LoadCredential =
          lib.optional (cfg.passwordFile != null) "password:${cfg.passwordFile}"
          ++ lib.optional (
            cfg.recoveryPassphraseFile != null
          ) "recovery-passphrase:${cfg.recoveryPassphraseFile}";

        ExecStart = "${cfg.package}/bin/matrix-embed ${matrixEmbedArgs}";

        Restart = "on-failure";
        RestartSec = "10s";
      };
    };

    systemd.services."matrix-embed-wireproxy" = lib.mkIf cfg.wireproxy.enable {
      description = "matrix-embed wireproxy companion";
      wantedBy = [ "multi-user.target" ];
      after = [ "matrix-embed-netns.service" ];
      requires = [ "matrix-embed-netns.service" ];
      bindsTo = [ "matrix-embed-netns.service" ];

      serviceConfig = hardening // {
        Type = "simple";
        DynamicUser = true;
        RuntimeDirectory = "matrix-embed-wireproxy";
        RuntimeDirectoryMode = "0700";

        NetworkNamespacePath = netnsPath;
        BindReadOnlyPaths = [ "${netnsResolvConf}:/etc/resolv.conf" ];

        LoadCredential = [
          "privatekey:${cfg.wireproxy.privateKeyFile}"
          "address:${cfg.wireproxy.addressFile}"
          "peerendpoint:${cfg.wireproxy.peer.endpointFile}"
          "peerpublickey:${cfg.wireproxy.peer.publicKeyFile}"
        ]
        ++ lib.optional (
          cfg.wireproxy.presharedKeyFile != null
        ) "presharedkey:${cfg.wireproxy.presharedKeyFile}";

        ExecStartPre = "${wireproxyConfigBuilder}";
        ExecStart = "${cfg.wireproxy.package}/bin/wireproxy --config %t/matrix-embed-wireproxy/wireproxy.conf";

        Restart = "on-failure";
        RestartSec = "10s";
      };
    };
  };
}
