from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parent.parent


class BootstrapFabricContractTest(unittest.TestCase):
    def test_supports_linux_local_peer_services(self) -> None:
        script = (ROOT / "scripts/bootstrap-fabric.sh").read_text()

        self.assertIn("Linux) local_service_manager=systemd", script)
        self.assertIn("systemctl --user enable --now", script)
        self.assertIn("distributed-workbench-peer-$peer_host.service", script)

    def test_initial_peer_bootstrap_does_not_immediately_kickstart(self) -> None:
        script = (ROOT / "scripts/bootstrap-fabric.sh").read_text()
        install_peer = script[script.index("install_peer_service()") : script.index("reconcile_local_peer_services()")]

        self.assertIn('launchctl bootstrap "$domain" "$plist"', install_peer)
        self.assertNotIn("launchctl kickstart", install_peer)

    def test_peer_plist_uses_a_unique_same_directory_temporary(self) -> None:
        script = (ROOT / "scripts/bootstrap-fabric.sh").read_text()
        install_peer = script[script.index("install_peer_service()") : script.index("reconcile_local_peer_services()")]

        self.assertIn('plist_temporary=$(mktemp "$plist.tmp.XXXXXX")', install_peer)
        self.assertIn('>"$plist_temporary"', install_peer)
        self.assertIn('mv "$plist_temporary" "$plist"', install_peer)
        self.assertNotIn('>"$plist.tmp"', install_peer)

    def test_reconnect_still_restarts_an_established_peer(self) -> None:
        script = (ROOT / "scripts/bootstrap-fabric.sh").read_text()

        self.assertIn(
            'launchctl kickstart -k "gui/$(id -u)/dev.distributed-workbench.peer.$reconnect_host"',
            script,
        )
        self.assertIn(
            'systemctl --user restart "distributed-workbench-peer-$reconnect_host.service"',
            script,
        )
        self.assertIn('wait_peer_generation "$reconnect_host" "$before_generation"', script)

    def test_repair_reconciles_release_drift_after_verify_failure(self) -> None:
        script = (ROOT / "scripts/repair-fabric.sh").read_text()
        fallback = script[script.index("verify-only failed; delegating reconciliation") :]

        self.assertIn('run_with_timeout "$timeout_seconds" "$bootstrap"', fallback)
        self.assertIn('--version "$version"', fallback)
        self.assertNotIn("--skip-release-install", fallback)

    def test_windows_controller_calls_use_stdin_rpc_for_json_safety(self) -> None:
        script = (ROOT / "scripts/bootstrap-fabric.sh").read_text()
        windows_call = script[script.index("windows_call()") : script.index("remote_call()")]

        self.assertIn('"apiVersion":"workbench.dev/v1"', windows_call)
        self.assertIn("call-stdin", windows_call)
        self.assertIn("RedirectStandardInput", windows_call)
        self.assertNotIn("Start-Process -FilePath", windows_call)

    def test_windows_install_registers_executor_through_stdin_rpc(self) -> None:
        script = (ROOT / "scripts/install-windows.ps1").read_text()
        registration = script[script.index("$registrationParams") :]

        self.assertIn('action = "executor.register"', registration)
        self.assertIn("call-stdin", registration)
        self.assertIn("RedirectStandardInput", registration)
        self.assertNotIn("$env:ComSpec", registration)
        self.assertIn("function Test-LocalSocketReady", script)
        self.assertIn("catch {", script)

    def test_windows_cleanup_is_idempotent(self) -> None:
        script = (ROOT / "scripts/bootstrap-fabric.sh").read_text()

        self.assertIn(
            "Remove-Item './install-distributed-workbench.ps1' -Force -ErrorAction SilentlyContinue",
            script,
        )

    def test_os_installers_isolate_canary_namespaces(self) -> None:
        linux = (ROOT / "scripts/install-linux-user.sh").read_text()
        macos = (ROOT / "scripts/install-macos-app.sh").read_text()
        windows = (ROOT / "scripts/install-windows.ps1").read_text()

        self.assertIn("DISTRIBUTED_WORKBENCH_NAMESPACE", linux)
        self.assertIn('installed_binary=$HOME/.local/bin/workbench$suffix', linux)
        self.assertIn('controller_service=distributed-workbench$suffix-controller.service', linux)
        self.assertIn("DISTRIBUTED_WORKBENCH_NAMESPACE", macos)
        self.assertIn('agent_label=dev.distributed-workbench$suffix.macos-agent', macos)
        self.assertIn('bundle_identifier=dev.distributed-workbench$suffix.macos-agent', macos)
        self.assertIn('[string]$Namespace = "stable"', windows)
        self.assertIn('$controllerService = "DistributedWorkbench" + $serviceNamespace + "Controller"', windows)
        self.assertIn('if ($Namespace -eq "stable") { "" }', windows)

    def test_prune_state_accepts_only_valid_managed_namespaces(self) -> None:
        script = ROOT / "scripts" / "prune-state.sh"
        with tempfile.TemporaryDirectory() as temporary:
            accepted = Path(temporary) / "distributed-workbench-canary"
            accepted.mkdir()
            result = subprocess.run(
                ["/bin/sh", str(script), "--state-root", str(accepted)],
                text=True,
                capture_output=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            rejected = Path(temporary) / "unmanaged-canary"
            rejected.mkdir()
            result = subprocess.run(
                ["/bin/sh", str(script), "--state-root", str(rejected)],
                text=True,
                capture_output=True,
            )
            self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
