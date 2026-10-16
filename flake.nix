{
  description = "rho - an ACP native agent";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          # The Angular client (web/), served by `rho daemon --listen`.
          web = pkgs.buildNpmPackage {
            pname = "rho-web";
            version = "0.1.0";
            src = pkgs.lib.cleanSourceWith {
              src = ./web;
              filter =
                path: _:
                !builtins.elem (baseNameOf path) [
                  "node_modules"
                  "dist"
                  ".angular"
                ];
            };
            nodejs = pkgs.nodejs_26;
            npmDepsHash = "sha256-3svFTdJb/owfTGStwnzUcRBhOhCed/U/MB8QxguIdWQ=";
            NG_CLI_ANALYTICS = "false";
            # sass-embedded ships a prebuilt `dart`, linked for FHS systems.
            nativeBuildInputs = pkgs.lib.optional pkgs.stdenv.isLinux pkgs.autoPatchelfHook;
            preBuild = pkgs.lib.optionalString pkgs.stdenv.isLinux "autoPatchelf node_modules/sass-embedded-linux-*";
            installPhase = "cp -r dist/web/browser $out";
          };
        in
        {
          inherit web;
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "rho";
            version = "0.1.0";
            src = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              # Git dependencies (not on crates.io).
              outputHashes."gitleaks-rs-0.1.0" = "sha256-yfl3w3STqaTWpetVkDJnvH+46aKI4yUBbOgBfyRNlZE=";
            };
            cargoBuildFlags = [ "-p" "rho-agent" ];
            # Memory/History embeddings: link onnxruntime from nixpkgs; the ld wrapper adds it to the rpath.
            buildInputs = [ pkgs.onnxruntime ];
            ORT_LIB_LOCATION = "${pkgs.onnxruntime}/lib";
            ORT_PREFER_DYNAMIC_LINK = "1";
            # Compiled in as the default web client dir (`acp_ws::web_dir`).
            RHO_WEB_DIR = "${web}";
            # Tests run in the dev shell; some need the network or a writable home.
            doCheck = false;
          };
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              clippy
              rustfmt
              rust-analyzer
              onnxruntime
              # web/ (Angular ACP client)
              nodejs_26
            ];
            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
            ORT_LIB_LOCATION = "${pkgs.onnxruntime}/lib";
            ORT_PREFER_DYNAMIC_LINK = "1";
          };
        }
      );
    };
}
