import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('reconcile', Path(__file__).resolve().parents[1] / 'scripts/reconcile-fabric.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

class ManifestExecution(unittest.TestCase):
    def test_identity_transport_and_roots_remain_per_node(self):
        manifest = {'initiatorNode':'laptop', 'nodes':[
            {'id':'laptop','platform':'macos','allowRoots':['${user.home}/Workspace']},
            {'id':'desk-a','platform':'windows','connection':{'sshAlias':'ssh-a'},'allowRoots':['D:/one']},
            {'id':'desk-b','platform':'windows','connection':{'sshAlias':'ssh-b'},'allowRoots':['E:/two']},
            {'id':'build','platform':'linux','connection':{'sshAlias':'ssh-c'},'allowRoots':['/srv/custom','${user.home}/src']},
        ]}
        command = module.compile_plan(manifest, '0.6.66')
        for identity, alias in [('desk-a','ssh-a'),('desk-b','ssh-b'),('build','ssh-c')]:
            i = command.index(alias)
            self.assertEqual(command[i-2:i+1], ['--node-transport',identity,alias])
        for identity, root in [('desk-a','D:/one'),('desk-b','E:/two'),('build','/srv/custom')]:
            i = command.index(root)
            self.assertEqual(command[i-2:i+1], ['--node-allow-root',identity,root])
        manifest['nodes'][-1]['allowRoots'] = ['/srv/$(touch bad)']
        with self.assertRaises(ValueError): module.compile_plan(manifest, '0.6.66')

    def test_unsupported_supervisor_rejected_before_install(self):
        manifest = {'initiatorNode':'laptop','nodes':[
            {'id':'laptop','platform':'macos','allowRoots':['/workspace']},
            {'id':'remote','platform':'macos','connection':{'sshAlias':'mac'},'allowRoots':['/workspace']}]}
        with self.assertRaisesRegex(ValueError, 'cannot supervise'):
            module.compile_plan(manifest,'0.6.66')
