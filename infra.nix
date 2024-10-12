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

  systemd = {
    services.wtfps-crawl = {
      after = [ "network.target" ];
      requires = [ "postgresql.service" ];
      wantedBy = [ "multi-user.target" ];

      script = ''
        ${pkgs.wtfps}/bin/wtfps explore crawl
      '';

      environment.DATABASE_URL = "postgres:///wtfps";

      serviceConfig = {
        Type = "simple";

        DynamicUser = true;
        User = "wtfps";
        Restart = "on-failure";
      };
    };

    services.wtfps-todo = {
      after = [ "network.target" ];
      requires = [ "postgresql.service" ];
      wantedBy = [ "multi-user.target" ];

      script = ''
        ${pkgs.wtfps}/bin/wtfps explore todo
      '';

      environment.DATABASE_URL = "postgres:///wtfps";

      serviceConfig = {
        Type = "simple";

        DynamicUser = true;
        User = "wtfps";
        Restart = "on-failure";
      };
    };

    services.wtfps-cn-crawl = {
      after = [ "network.target" ];
      requires = [ "postgresql.service" ];
      wantedBy = [ "multi-user.target" ];

      script = ''
        ${pkgs.wtfps}/bin/wtfps --china explore crawl
      '';

      environment.DATABASE_URL = "postgres:///wtfps_cn";

      serviceConfig = {
        Type = "simple";

        DynamicUser = true;
        User = "wtfps_cn";
        Restart = "on-failure";
      };
    };

    services.wtfps-cn-todo = {
      after = [ "network.target" ];
      requires = [ "postgresql.service" ];
      wantedBy = [ "multi-user.target" ];

      script = ''
        ${pkgs.wtfps}/bin/wtfps --china explore todo
      '';

      environment.DATABASE_URL = "postgres:///wtfps_cn";

      serviceConfig = {
        Type = "simple";

        DynamicUser = true;
        User = "wtfps_cn";
        Restart = "on-failure";
      };
    };
  };
}

