"""Exercise installation rendering with fake supervisors; never touch live services."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent

class ClipboardInstallTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='clipboard-install-test-')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        bin_dir = self.root / 'tools'
        bin_dir.mkdir()
        for name in ('systemctl', 'tmux', 'Xvfb'):
            self.script(bin_dir / name, '#!/bin/sh\nexit 0\n')
        self.script(bin_dir / 'xauth', '#!/bin/sh\ncase $3 in add) printf cookie >> "$2";; list) cat "$2";; esac\n')
        self.binary = self.root / 'workbench-fixture'
        self.script(self.binary, '#!/bin/sh\nprintf "workbench 0.6.46\\n"\n')
        # Only the subprocess sees this temporary HOME; real files/services are untouched.
        self.env = dict(os.environ, HOME=str(self.root), XDG_CONFIG_HOME=str(self.root / 'config'),
                        XDG_STATE_HOME=str(self.root / 'state'), PATH=str(bin_dir) + ':/usr/bin:/bin')
        self.env.pop('DISTRIBUTED_WORKBENCH_NAMESPACE', None)
        self.env.pop('DISTRIBUTED_WORKBENCH_CLIPBOARD_DISPLAY', None)

    @staticmethod
    def script(path, text):
        path.write_text(text)
        path.chmod(0o755)

    def install(self, display=None):
        env = dict(self.env)
        if display is not None:
            env['DISTRIBUTED_WORKBENCH_CLIPBOARD_DISPLAY'] = display
        return subprocess.run(['/bin/sh', 'scripts/install-linux-user.sh', str(self.binary), 'test-executor'],
                              cwd=ROOT, env=env, capture_output=True, text=True, timeout=20)

    def test_display_auth_service_and_plain_shell_environment_survive_upgrade(self):
        result = self.install(':98')
        self.assertEqual(result.returncode, 0, result.stderr)
        unit = (self.root / 'config/systemd/user/distributed-workbench-executor.service').read_text()
        self.assertIn('Requires=distributed-workbench-clipboard-x11.service\nAfter=', unit)
        self.assertIn('Environment=DISPLAY=:98\nEnvironment=XAUTHORITY=', unit)
        self.assertNotIn('@CLIPBOARD', unit)
        auth = self.root / 'state/distributed-workbench/clipboard-Xauthority'
        self.assertEqual(auth.stat().st_mode & 0o777, 0o600)
        display = (self.root / 'config/systemd/user/distributed-workbench-clipboard-x11.service').read_text()
        self.assertIn('-nolisten tcp -noreset -auth ', display)
        before = auth.read_bytes()
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(auth.read_bytes(), before)
        self.assertIn('DISPLAY=:98', (self.root / 'config/distributed-workbench/clipboard-env').read_text())

    def test_invalid_display_fails_before_installing_any_binary(self):
        result = self.install(':98;touch nope')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root / '.local/bin/workbench').exists())

    def test_opt_out_does_not_install_display(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.root / 'config/distributed-workbench/clipboard-env').exists())
        unit = (self.root / 'config/systemd/user/distributed-workbench-executor.service').read_text()
        self.assertNotIn('@CLIPBOARD', unit)
        self.assertNotIn('Environment=DISPLAY=', unit)

if __name__ == '__main__':
    unittest.main()
