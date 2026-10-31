{
  description = "rho - an ACP native agent";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # zg for SemanticSearch, built below.
    zvec-grep = {
      url = "github:zvec-ai/zvec-grep/2d7d2ac220d1baf840694d937fea7054d091e33d";
      flake = false;
    };
  };

  outputs =
    { nixpkgs, zvec-grep, ... }:
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
            npmDepsHash = "sha256-eyH4FAoyOFrKmHV9xPK8hWm8Be8kNw7HHPr426l3cLk=";
            NG_CLI_ANALYTICS = "false";
            # sass-embedded ships a prebuilt `dart`, linked for FHS systems.
            nativeBuildInputs = pkgs.lib.optional pkgs.stdenv.hostPlatform.isLinux pkgs.autoPatchelfHook;
            preBuild = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux "autoPatchelf node_modules/sass-embedded-linux-*";
            installPhase = "cp -r dist/web/browser $out";
          };
          target = pkgs.stdenv.hostPlatform.rust.rustcTarget;
          # zvec-rust-sys would download this in its build script; the nix sandbox has no network.
          zvec = pkgs.stdenv.mkDerivation {
            pname = "zvec-prebuilt";
            version = "0.7.2";
            src = pkgs.fetchurl {
              url = "https://github.com/zvec-ai/zvec-rust/releases/download/v0.7.2/zvec-prebuilt-${target}.tar.gz";
              hash =
                {
                  x86_64-unknown-linux-gnu = "sha256-EjveZO2LquXqgTkHr1qzV1ETKB64H8pTEPKKD+T/uSs=";
                  aarch64-unknown-linux-gnu = "sha256-fIrdghvCR+zbemiNRWrmD03KkQeOlCCEMAdXvy2dkfs=";
                  aarch64-apple-darwin = "sha256-X9Etdlm0lb9rBroXOHbkANrh3BnV4E6EaDgcgoZ+pH8=";
                }
                .${target};
            };
            sourceRoot = ".";
            nativeBuildInputs = pkgs.lib.optional pkgs.stdenv.hostPlatform.isLinux pkgs.autoPatchelfHook;
            buildInputs = [ pkgs.stdenv.cc.cc.lib ];
            installPhase = "mkdir -p $out && cp -r . $out";
          };
          # The zvec-grep CLI, as in zvec-grep's rust/flake.nix.
          zg = pkgs.rustPlatform.buildRustPackage {
            pname = "zg";
            version = "0.0.1";
            src = "${zvec-grep}/rust";
            cargoLock.lockFile = "${zvec-grep}/rust/Cargo.lock";
            cargoBuildFlags = [ "-p" "zg" ];
            doCheck = false;
            nativeBuildInputs = with pkgs; [
              rustPlatform.bindgenHook
              cmake
              pkg-config
              perl
              git
            ];
            dontUseCmakeConfigure = true;
            ZVEC_LIB_DIR = "${zvec}";
            ZVEC_AUTO_BUILD = "0";
            # Link nixpkgs onnxruntime instead of letting ort-sys download its static build.
            ORT_LIB_PATH = "${pkgs.onnxruntime}/lib";
            ORT_PREFER_DYNAMIC_LINK = "1";
          };
        in
        {
          inherit web zg;
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
            # SemanticSearch looks up `zg` on the PATH.
            nativeBuildInputs = [ pkgs.makeWrapper ];
            postInstall = "wrapProgram $out/bin/rho --prefix PATH : ${zg}/bin";
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
