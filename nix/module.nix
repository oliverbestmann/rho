self:
{
  lib,
  config,
  pkgs,
  ...
}:

let
  cfg = config.services.rho;
in
{
  options.services.rho = {
    enable = lib.mkEnableOption "the rho coding agent daemon as a user service";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      description = "The rho package.";
    };
    binary = lib.mkOption {
      type = lib.types.str;
      default = lib.getExe' cfg.package "rho";
      defaultText = lib.literalExpression ''lib.getExe' cfg.package "rho"'';
      example = "/home/alice/.local/bin/rho";
      description = "Path to the rho binary, e.g. a local build instead of the package.";
    };
    mode = lib.mkOption {
      type = lib.types.enum [
        "daemon"
        "proxy"
      ];
      default = "daemon";
      description = "`daemon`: ACP, web client and model API. `proxy`: only the model API.";
    };
    user = lib.mkOption {
      type = lib.types.str;
      description = "The user the daemon runs for.";
    };
    listen = lib.mkOption {
      type = lib.types.str;
      default = "0.0.0.0:8766";
      description = "Address for `--listen`.";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.user.services.rho = {
      description = "rho coding agent daemon";
      wantedBy = [ "default.target" ];
      unitConfig.ConditionUser = cfg.user;
      # home-manager packages plus system packages, for the tools the agent runs
      path = [
        "/etc/profiles/per-user/${cfg.user}"
        "/run/current-system/sw"
      ];
      serviceConfig = {
        ExecStart = "${cfg.binary} ${cfg.mode} --listen ${cfg.listen}";
        Restart = "on-failure";
        RestartSec = 5;
      };
    };
  };
}
