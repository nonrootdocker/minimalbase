{
  description = "minimalbase-ng";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs";
  };

  outputs = { self, nixpkgs }:
  let
    system = "x86_64-linux";
    pkgs = import nixpkgs { inherit system; };

container-init = pkgs.rustPlatform.buildRustPackage {
  name = "container-init";
  version = "0.2.0";

  src = pkgs.lib.cleanSource ./rust-init;

  cargoLock = {
    lockFile = ./rust-init/Cargo.lock;
  };

  nativeBuildInputs = with pkgs; [
    pkg-config
  ];
};

  in {
    packages.${system} = {
      container-init = container-init;

      base-image = pkgs.dockerTools.buildImage {
        name = "minimalbase-ng";
        tag = "latest";

        copyToRoot = pkgs.buildEnv {
          name = "root";
          paths = with pkgs; [
            coreutils
            tzdata
            cacert
            container-init
          ];
        };

        config = {
          Entrypoint = [ "${container-init}/bin/container-init" ];

          Env = [
            "TZ=UTC"
            "LANG=en_US.UTF-8"
          ];
        };
      };
    };
  };
}
