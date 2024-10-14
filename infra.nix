{ pkgs, ... }:


{
  services.postgresql = {
    ensureDatabases = [
      "wtfps"
      "wtfps_cn"
    ];
    ensureUsers = [
      { name = "wtfps"; ensureDBOwnership = true; }
      { name = "wtfps_cn"; ensureDBOwnership = true; }
    ];
  };

  systemd.services =
    let
      mkService = db: args: {
        after = [ "network.target" ];
        requires = [ "postgresql.service" ];
        wantedBy = [ "multi-user.target" ];

        script = ''
          ${pkgs.wtfps}/bin/wtfps ${args} | ${pkgs.mosquitto}/bin/mosquitto_pub -L "mqtt://192.168.1.3/${db}" -l
        '';

        environment.DATABASE_URL = "postgres:///${db}";

        serviceConfig = {
          Type = "simple";

          DynamicUser = true;
          User = db;
          Restart = "on-failure";
        };
      };
    in

    {
      wtfps-crawl = mkService "wtfps" "explore crawl";
      wtfps-todo = mkService "wtfps" "explore todo";
      wtfps-crawl-china = mkService "wtfps_cn" "--china explore crawl";
      wtfps-todo-china = mkService "wtfps_cn" "--china explore todo";
    };
}

