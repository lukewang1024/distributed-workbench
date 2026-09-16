#!/usr/bin/env python3
"""Execute the public Fabric declaration through the native bootstrap authority."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess


def compile_plan(manifest, version, *, verify=False, skip_install=False):
    initiator = manifest['initiatorNode']
    nodes = manifest['nodes']
    command = [str(Path(__file__).with_name('bootstrap-fabric.sh')), '--version', version, '--local-id', initiator]
    specs = []
    aliases = set()
    for node in nodes:
        identity = node['id']
        if not re.fullmatch(r'[A-Za-z0-9._-]+', identity):
            raise ValueError('invalid node identity')
        local = identity == initiator
        platform = node['platform']
        if (local and platform not in {'macos', 'linux'}) or (not local and platform not in {'linux', 'windows'}):
            raise ValueError(f'bootstrap cannot supervise {platform} node {identity}')
        if not local:
            alias = node['connection']['sshAlias']
            if not re.fullmatch(r'[A-Za-z0-9._-]+', alias):
                raise ValueError('invalid SSH alias')
            if alias in aliases:
                raise ValueError('selected nodes must use distinct SSH aliases')
            aliases.add(alias)
            command += ['--node-transport', identity, alias]
            specs.append(('windows:' if platform == 'windows' else '') + identity)
        for root in node['allowRoots']:
            # Shell and PowerShell transports currently use literal root arguments.
            suffix = root.removeprefix('${user.home}/')
            if any(c in suffix for c in "\"'`$|\n\r\x00"):
                raise ValueError(f'unsupported root characters on {identity}')
            if platform == 'windows':
                if not re.match(r'^[A-Za-z]:[\\/]', root):
                    raise ValueError('Windows roots must be absolute')
            elif not (root.startswith('/') or root.startswith('${user.home}/')):
                raise ValueError('POSIX roots must be absolute or home-relative')
            if local:
                root = root.replace('${user.home}', str(Path.home()))
                command += ['--local-allow-root', root]
            else:
                command += ['--node-allow-root', identity, root]
    if not specs:
        raise ValueError('bootstrap requires at least one remote node')
    if verify:
        command.append('--verify-only')
    if skip_install:
        command.append('--skip-release-install')
    return command + specs


def verify_roots(binary, manifest):
    for node in manifest['nodes']:
        executor = node['id'] + ('-native' if node['platform'] == 'windows' else '-rust')
        reply = json.loads(subprocess.check_output([binary, 'call', 'executor.call', json.dumps({
            'executorId': executor, 'action': 'status', 'params': {}})], text=True))
        if not reply.get('ok'):
            raise RuntimeError(f'executor status failed: {executor}')
        status = reply['result']
        roots = status.get('configuredRoots')
        if roots is None:
            raise RuntimeError(f'{executor} cannot report configured roots; upgrade core first')
        home = status.get('userHome') or ''
        expected = [root.replace('${user.home}', home) for root in node['allowRoots']]
        def normalize(root):
            if node['platform'] == 'windows':
                return root.replace('\\', '/').rstrip('/').casefold()
            return root.rstrip('/')
        if {normalize(root) for root in roots} != {normalize(root) for root in expected}:
            raise RuntimeError(f'allowRoots drift on {executor}: expected {expected!r}, running {roots!r}')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--file', required=True)
    parser.add_argument('--version', required=True)
    parser.add_argument('--plan', action='store_true')
    parser.add_argument('--verify-only', action='store_true')
    parser.add_argument('--skip-release-install', action='store_true')
    args = parser.parse_args()
    binary = os.environ.get('DISTRIBUTED_WORKBENCH_BINARY', 'workbench')
    manifest = json.loads(subprocess.check_output([binary, 'fabric', 'validate', '--file', args.file], text=True))
    command = compile_plan(manifest, args.version, verify=args.verify_only, skip_install=args.skip_release_install)
    if args.plan:
        print(json.dumps({'manifest': manifest, 'command': command}, indent=2))
        return
    if args.skip_release_install and not args.verify_only:
        try:
            verify_roots(binary, manifest)
        except (RuntimeError, subprocess.CalledProcessError):
            command.remove('--skip-release-install')
    subprocess.run(command, check=True)
    verify_roots(binary, manifest)


if __name__ == '__main__':
    main()
