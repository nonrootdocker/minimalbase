{
  description = "minimalbase-ng";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";
  };

  outputs = { self, nixpkgs }:
  let
    system = "x86_64-linux";
    pkgs = import nixpkgs { inherit system; };

    container-init = pkgs.rustPlatform.buildRustPackage {
      pname = "container-init";
      version = "0.2.0";

      src = ./rust-init;

      cargoLock = {
        lockFile = ./rust-init/Cargo.lock;
      };
    };

  in {
    packages.${system} = {
      init = container-init;

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
          Cmd = [ "/app/main-process" ];

          Env = [
            "TZ=UTC"
            "LANG=en_US.UTF-8"
          ];
        };
      };
    };
  };
}
