import importlib.util
import json
from pathlib import Path
import tempfile
import subprocess
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('supervise', Path(__file__).resolve().parents[1] / 'scripts/supervise-node.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class Supervision(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='dwb-', dir='/tmp')
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        binary = self.root / 'binary'
        binary.write_text('binary fixture')
        self.config = dict(nodeId='headless', binary=str(binary), supervisor='/usr/bin/supervisord',
                           stateRoot=str(self.root), allowRoots=['/workspace/a b'])

    def test_plan_does_not_create_files(self):
        node = module.Node(self.config)
        before = list(self.root.iterdir())
        self.assertTrue(node.reconcile('plan')['changedFiles'])
        self.assertEqual(before, list(self.root.iterdir()))

    def test_equal_content_preserves_inode_and_mtime(self):
        path = self.root / 'state'
        self.assertTrue(module.atomic_write(path, 'desired'))
        before = path.stat()
        self.assertFalse(module.atomic_write(path, 'desired'))
        self.assertEqual(before.st_ino, path.stat().st_ino)
        self.assertEqual(before.st_mtime_ns, path.stat().st_mtime_ns)

    def test_unchanged_ensure_does_not_register_or_start_again(self):
        node = module.Node(self.config)
        for path, (content, mode) in node.desired().items():
            module.atomic_write(path, content, mode)
        status = dict(registered=True, pids={'controller': 1, 'executor': 2})
        with patch.object(node, 'listening', return_value=True), patch.object(node, 'status', return_value=status), patch.object(node, 'run') as run:
            self.assertEqual(node.reconcile('ensure')['changedFiles'], [])
            run.assert_not_called()

    def test_running_drift_refuses_mutation(self):
        node = module.Node(self.config)
        with patch.object(node, 'listening', return_value=True):
            with self.assertRaisesRegex(RuntimeError, 'drain and stop'):
                node.reconcile('ensure')
        self.assertFalse(node.ini.exists())

    def test_pid_zero_not_healthy(self):
        node = module.Node(self.config)
        with patch.object(node, 'listening', return_value=True), patch.object(node, 'ctl', return_value='0'):
            with self.assertRaisesRegex(RuntimeError, 'missing child'):
                node.status()

    def test_stop_children_before_shutdown(self):
        node = module.Node(self.config)
        self.assertIn('stopsignal=TERM\nstopwaitsecs=10', node.desired()[node.ini][0])
        with patch.object(node, 'listening', side_effect=[True, False]), patch.object(node, 'ctl', return_value='123') as ctl:
            self.assertEqual(node.reconcile('stop'), {'stopped': True})
            self.assertEqual([c.args for c in ctl.call_args_list],
                             [('pid', 'executor'), ('stop', 'executor'), ('pid', 'controller'),
                              ('stop', 'controller'), ('shutdown',)])

    def test_binary_change_is_drift(self):
        node = module.Node(self.config)
        for path, (content, mode) in node.desired().items():
            module.atomic_write(path, content, mode)
        Path(self.config['binary']).write_text('different binary')
        self.assertIn(str(self.root / 'installed.json'), node.reconcile('plan')['changedFiles'])

    def test_argument_boundaries_and_bad_paths(self):
        node = module.Node(self.config)
        self.assertIn("'/workspace/a b'", node.desired()[self.root / 'executor.sh'][0])
        subprocess.run(['/bin/sh', '-n'], input=node.desired()[self.root / 'executor.sh'][0], text=True, check=True)
        for path in ['relative', '/tmp/inject\nvalue', '/tmp/%(interpolation)s']:
            with self.assertRaises(ValueError):
                module.Node(dict(self.config, stateRoot=path))

    def test_peer_uses_explicit_local_endpoints_and_gateway_alias(self):
        peer = dict(id='gateway', binary='/opt/workbench', stateRoot='/tmp/gateway', sshAlias='gateway-ssh')
        node = module.Node(dict(self.config, peers=[peer]))
        command = node.commands()['peer-gateway']
        self.assertEqual(command[command.index('--host')+1], 'gateway-ssh')
        self.assertEqual(command[command.index('--local-controller-socket')+1], str(self.root/'controller.sock'))
        with self.assertRaisesRegex(ValueError, 'duplicate'):
            module.Node(dict(self.config, peers=[peer, peer]))

    def test_peer_registration_conflict_is_not_overwritten(self):
        peer = dict(id='gateway', binary='/opt/workbench', stateRoot='/tmp/gateway', sshAlias='gateway-ssh')
        node = module.Node(dict(self.config, peers=[peer]))
        directory = self.root/'peers/gateway'
        directory.mkdir(parents=True)
        (directory/'status.json').write_text(json.dumps({'state':'ready','peerId':'gateway','generation':1}))
        status = {'controller':{'id':'headless'},'controllers':[{'id':'gateway','endpoint':{'socket':'/unrelated'}}]}
        with patch.object(node,'rpc_at',return_value=status) as rpc:
            with self.assertRaisesRegex(RuntimeError,'registration conflict'):
                node.peer_status(mutate=True)
            self.assertEqual(rpc.call_count,1)
