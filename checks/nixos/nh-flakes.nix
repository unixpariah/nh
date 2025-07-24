{
  testers,
  writeText,
  ...
}:
testers.runNixOSTest {
  name = "nh-nixos-test";
  nodes.machine = {
    lib,
    pkgs,
    ...
  }: {
    imports = [
      ../vm.nix
    ];

    nix.settings = {
      substituters = lib.mkForce [];
      hashed-mirrors = null;
      connect-timeout = 1;
    };

    # Indicate parent config
    environment.systemPackages = [
      (pkgs.writeShellScriptBin "parent" "")
    ];

    programs.nh = {
      enable = true;
      flake = "/etc/nixos";
    };

    users.groups.alice = {};
    users.users.alice = {
      isNormalUser = true;
      password = "";
    };
  };

  testScript = let
    newConfig =
      writeText "configuration.nix" # nix

      ''
        { lib, pkgs, ... }: {
          imports = [
            ./hardware-configuration.nix
            <nixpkgs/nixos/modules/testing/test-instrumentation.nix>
          ];

          boot.loader.grub = {
            enable = true;
            device = "/dev/vda";
            forceInstall = true;
          };

          documentation.enable = false;

          environment.systemPackages = [
            (pkgs.writeShellScriptBin "parent" "")
          ];


          specialisation.foo = {
            inheritParentConfig = true;

            configuration = {...}: {
              environment.etc."specialisation".text = "foo";
            };
          };

          specialisation.bar = {
            inheritParentConfig = true;

            configuration = {...}: {
              environment.etc."specialisation".text = "bar";
            };
          };

          nix.settings.experimental-features = ["nix-command" "flakes"];
        }
      '';
  in
    # python
    ''
      machine.start()
      machine.succeed("udevadm settle")
      machine.wait_for_unit("multi-user.target")

      machine.succeed("nixos-generate-config --flake")
      machine.copy_from_host("${newConfig}", "/etc/nixos/configuration.nix")

      with subtest("Switch to the base system"):
        machine.succeed("su alice -c 'nh os switch --no-nom'")
        machine.succeed("parent")
        machine.fail("cat /etc/specialisation/text | grep 'foo'")
        machine.fail("cat /etc/specialisation/text | grep 'bar'")

      with subtest("Switch to the foo system"):
        machine.succeed("su alice -c 'nh os switch --no-nom --specialisation foo'")
        machine.succeed("parent")
        machine.succeed("cat /etc/specialisation/text | grep 'foo'")
        machine.fail("cat /etc/specialisation/text | grep 'bar'")

      with subtest("Switch to the bar system"):
        machine.succeed("su alice -c 'nh os switch --no-nom --specialisation bar'")
        machine.succeed("parent")
        machine.fail("cat /etc/specialisation/text | grep 'foo'")
        machine.succeed("cat /etc/specialisation/text | grep 'bar'")

      with subtest("Switch into specialization using `nh os test`"):
        machine.succeed("su alice -c 'nh os test --specialisation foo'")
        machine.succeed("parent")
        machine.succeed("foo")
        machine.fail("bar")

      # Other tests that test additional safeguards found in NH
      with subtest("Disallow running commands as root"):
        machine.fail("nh os build --no-nom")
    '';
}
