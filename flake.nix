{
  inputs = {
    nixpkgs.url = "github:joelkoen/nixpkgs";

    crane = {
      url = "github:ipetkov/crane";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { nixpkgs, crane, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        lib = pkgs.lib;
        craneLib = crane.lib.${system};
      in
      with pkgs;
      {
        packages.default = craneLib.buildPackage {
          src = lib.cleanSourceWith {
            src = craneLib.path ./.;
            filter = path: type:
              lib.hasSuffix ".sql" path
              || lib.hasSuffix ".proto" path
              || craneLib.filterCargoSources path type;
          };

          buildInputs = [ pkg-config protobuf ];
          nativeBuildInputs = [ openssl ];
        };

        devShells.default = mkShell {
          buildInputs = [
            sqlx-cli
          ];
        };
      }
    );
}

