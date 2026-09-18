import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


SCRIPT = Path(__file__).parents[1] / "scripts/deploy-computer-use-host.py"
SPEC = importlib.util.spec_from_file_location("deploy_computer_use_host", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ComputerUseEnvironmentTests(unittest.TestCase):
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
