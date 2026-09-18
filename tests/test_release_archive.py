"""Exercise the real packager so headless installations need no checkout."""
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class ReleaseArchive(unittest.TestCase):
    def test_headless_helpers_are_shipped(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            build = root / 'build'
            build.mkdir()
            (build / 'workbench').write_text('binary fixture')
            # The script-only contract also applies to archives without CU.
            output = root / 'dist'
            subprocess.run(['sh', str(ROOT / 'scripts/package-release.sh'),
                            'test', 'aarch64-linux-android', str(build), str(output)],
                           cwd=ROOT, check=True)
            with tarfile.open(next(output.glob('*.tar.gz'))) as archive:
                names = archive.getnames()
                for helper in ('supervise-node.py', 'restricted-peer.py', 'call-via.py'):
                    member = next((name for name in names if name.endswith('/scripts/' + helper)), None)
                    self.assertIsNotNone(member, helper)
                    self.assertEqual(archive.extractfile(member).read(),
                                     (ROOT / 'scripts' / helper).read_bytes())

    def test_linux_archive_contains_locks_but_no_language_runtime(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            build = root/'build'; build.mkdir()
            (build/'workbench').write_text('binary fixture')
            output = root/'dist'
            subprocess.run(['sh', str(ROOT/'scripts/package-release.sh'), 'test',
                            'x86_64-unknown-linux-musl', str(build), str(output)], cwd=ROOT, check=True)
            with tarfile.open(next(output.glob('*.tar.gz'))) as archive:
                names = archive.getnames()
                self.assertTrue(any(n.endswith('/computer-use/package-lock.json') for n in names))
                self.assertTrue(any(n.endswith('/scripts/trim-computer-use-runtime.mjs') for n in names))
                self.assertFalse(any('/node_modules' in n or n.endswith('/computer-use/node') or n.endswith('/computer-use/node.exe') for n in names))
                self.assertLess(sum(m.size for m in archive.getmembers()), 2*1024*1024)
