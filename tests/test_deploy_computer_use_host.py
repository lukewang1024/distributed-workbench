import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).parents[1] / "scripts/deploy-computer-use-host.py"
SPEC = importlib.util.spec_from_file_location("deploy_computer_use_host", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ComputerUseEnvironmentTests(unittest.TestCase):
    def test_desktop_policy_requires_explicit_windows_user_and_foreground(self):
        node = {'id':'test', 'platform':'windows', 'windowsDesktop':{'enabled':True,'user':'tester'}}
        self.assertEqual(MODULE.desired_environment(node), {'PI_COMPUTER_USE_HEADLESS':'false'})
        for bad in [dict(node, platform='linux'), dict(node, windowsDesktop={'enabled':True}),
                    dict(node, computerUseEnvironment={'PI_COMPUTER_USE_HEADLESS':'true'}),
                    dict(node, windowsDesktop={'enabled':'true','user':'tester'})]:
            with self.assertRaises(ValueError):
                MODULE.desired_environment(bad)

    def test_disabled_and_unmanaged_do_not_force_environment(self):
        node = {'id':'test','platform':'windows','windowsDesktop':{'enabled':False}}
        self.assertIsNone(MODULE.desired_environment(node))
        self.assertIn('-Disable -VerifyOnly', MODULE.desktop_command(node, True))
        self.assertIsNone(MODULE.desktop_command({'id':'test'}))

    def test_payload_is_stdin_not_a_windows_command_line(self):
        node = {'id':'test','connection':{'sshAlias':'test-host'}}
        with mock.patch.object(MODULE, 'run') as run:
            MODULE.powershell_node(node, '', 'x'*50000)
        self.assertLess(len(' '.join(run.call_args.args[0])), 1000)
        self.assertGreater(len(run.call_args.kwargs['input']), 50000)

    def test_environment_content_is_stable_and_string_typed(self):
        node = {
            "id": "windows",
            "computerUseEnvironment": {
                "PI_COMPUTER_USE_HEADLESS": "false",
                "DISPLAY": ":1",
            },
        }
        self.assertEqual(
            MODULE.environment_content(node),
            '{"DISPLAY":":1","PI_COMPUTER_USE_HEADLESS":"false"}\n',
        )
        with self.assertRaises(ValueError):
            MODULE.environment_content({
                "id": "windows",
                "computerUseEnvironment": {"PI_COMPUTER_USE_HEADLESS": False},
            })

    def test_local_environment_converges_and_detects_drift(self):
        node = {
            "id": "windows",
            "computerUseEnvironment": {"PI_COMPUTER_USE_HEADLESS": "false"},
        }
        with tempfile.TemporaryDirectory() as directory:
            MODULE.local_environment(directory, node, False)
            path = Path(directory) / "environment.json"
            self.assertEqual(json.loads(path.read_text()), node["computerUseEnvironment"])
            MODULE.local_environment(directory, node, True)
            path.write_text('{"PI_COMPUTER_USE_HEADLESS":"true"}\n')
            with self.assertRaisesRegex(RuntimeError, "CU environment drift"):
                MODULE.local_environment(directory, node, True)

    def test_omitted_environment_is_unmanaged(self):
        with tempfile.TemporaryDirectory() as directory:
            MODULE.local_environment(directory, {"id": "mac"}, False)
            self.assertFalse((Path(directory) / "environment.json").exists())


if __name__ == "__main__":
    unittest.main()
