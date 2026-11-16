{
  description = "pairdeck: twin-panel terminal file manager inspired by Midnight Commander (command: pdeck)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  # Linux only. `nix run github:portvex/pairdeck`, or add
  # `inputs.pairdeck.url = "github:portvex/pairdeck";` and use
  # `pairdeck.packages.${system}.default` in a NixOS or Home Manager configuration.
  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      manifest = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package;
    in
    {
      packages = forAllSystems (pkgs: {
        pairdeck = pkgs.rustPlatform.buildRustPackage {
          pname = "pairdeck";
          inherit (manifest) version;
          src = pkgs.lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [
            "--bin"
            "pdeck"
          ];
          nativeBuildInputs = [ pkgs.installShellFiles ];
          # The test suite starts sshd and other servers; CI runs it instead.
          doCheck = false;
          postInstall = ''
            $out/bin/pdeck --man > pdeck.1
            installManPage pdeck.1
            installShellCompletion --cmd pdeck \
              --bash <($out/bin/pdeck --completions bash) \
              --zsh <($out/bin/pdeck --completions zsh) \
              --fish <($out/bin/pdeck --completions fish)
          '';
          meta = {
            inherit (manifest) description;
            homepage = "https://github.com/portvex/pairdeck";
            license = pkgs.lib.licenses.mit;
            mainProgram = "pdeck";
            platforms = pkgs.lib.platforms.linux;
          };
        };
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.pairdeck;
      });

      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = "${self.packages.${pkgs.stdenv.hostPlatform.system}.default}/bin/pdeck";
        };
      });

      checks = forAllSystems (pkgs: {
        pairdeck = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      });
    };
}
