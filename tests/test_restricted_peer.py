import importlib.util
from pathlib import Path
import shlex
import unittest

spec = importlib.util.spec_from_file_location('restricted_peer', Path(__file__).resolve().parents[1] / 'scripts/restricted-peer.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class RestrictedPeer(unittest.TestCase):
    def setUp(self):
        self.config = dict(binary='/opt/workbench/bin/workbench', stateRoot='/home/user/.local/state/dwb',
                           nodeId='gateway', peerId='cli-1')

    def test_exact_peer_and_version(self):
        argv = module.allowed(self.config)
        self.assertEqual(module.authorize(self.config, shlex.join(argv)), argv)
        self.assertEqual(module.authorize(self.config, '/opt/workbench/bin/workbench --version'),
                         ['/opt/workbench/bin/workbench', '--version'])

    def test_rejects_shell_other_identity_and_other_socket(self):
        argv = module.allowed(self.config)
        for command in ['sh', '', 'id', shlex.join(argv) + '; id',
                        shlex.join(argv).replace('cli-1', 'other'),
                        shlex.join(argv).replace('controller.sock', 'different.sock')]:
            with self.assertRaises(ValueError):
                module.authorize(self.config, command)
