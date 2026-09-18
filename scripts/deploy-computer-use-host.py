#!/usr/bin/env python3
"""Stage a checked host-only artifact on declared Fabric nodes, without service restarts."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import urllib.request
import time
import uuid

ROOT = Path(__file__).resolve().parent.parent
COMPUTER_USE_ENVIRONMENT_KEYS = {
    'DISPLAY', 'XAUTHORITY', 'DBUS_SESSION_BUS_ADDRESS', 'AT_SPI_BUS_ADDRESS',
    'XDG_RUNTIME_DIR', 'PI_COMPUTER_USE_HEADLESS',
}

def run(command, **kwargs):
    return subprocess.run(command, check=True, text=True, **kwargs)

def desired_environment(node):
    if 'computerUseEnvironment' not in node:
        return None
    environment = node['computerUseEnvironment']
    if not isinstance(environment, dict) or set(environment) - COMPUTER_USE_ENVIRONMENT_KEYS:
        raise ValueError(f"invalid computerUseEnvironment on {node['id']}")
    if any(not isinstance(value, str) for value in environment.values()):
        raise ValueError(f"computerUseEnvironment values must be strings on {node['id']}")
    headless = environment.get('PI_COMPUTER_USE_HEADLESS')
    if headless is not None and headless not in {'true', 'false'}:
        raise ValueError(f"PI_COMPUTER_USE_HEADLESS must be 'true' or 'false' on {node['id']}")
    return environment

def environment_content(node):
    environment = desired_environment(node)
    return None if environment is None else json.dumps(
        environment, sort_keys=True, separators=(',', ':')) + '\n'

def local_environment(state, node, verify):
    expected = desired_environment(node)
    if expected is None:
        return
    path = Path(state) / 'environment.json'
    if verify:
        try:
            actual = json.loads(path.read_text())
        except (OSError, json.JSONDecodeError) as error:
            raise RuntimeError(f"CU environment drift on {node['id']}: {error}") from error
        if actual != expected:
            raise RuntimeError(
                f"CU environment drift on {node['id']}: expected {expected!r}, found {actual!r}")
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + '.tmp-' + uuid.uuid4().hex)
    temporary.write_text(environment_content(node))
    os.replace(temporary, path)

def deploy_node(binary, node, initiator, artifact, digest, verify=False):
    executor = node['id'] + ('-native' if node['platform'] == 'windows' else '-rust')
    reply = json.loads(subprocess.check_output([binary, 'call', 'executor.call', json.dumps({
        'executorId': executor, 'action': 'status', 'params': {}})], text=True))
    if not reply.get('ok'):
        raise RuntimeError(f'cannot inspect {executor}')
    state = reply['result'].get('computerUseStateRoot')
    if not state:
        raise RuntimeError(f'{executor} needs a core upgrade before independent host installation')
    selected = reply['result'].get('computerUse', {}).get('selectedArtifactDigest')
    if verify and selected != digest:
        raise RuntimeError(f'CU host selection drift on {executor}: {selected!r}')
    if node['id'] == initiator:
        if verify:
            local_environment(state, node, True)
            print(json.dumps({'node':node['id'], 'hostArtifactDigest':selected,
                              'computerUseEnvironment':desired_environment(node)}))
            return
        runtime = Path((Path(state) / 'runtime-root').read_text().strip())
        run([str(runtime / 'node'), str(ROOT / 'scripts/install-computer-use-host.mjs'),
             str(artifact), digest, state, str(runtime.parent.parent)])
        local_environment(state, node, False)
        return
    alias = node['connection']['sshAlias']
    # Keep remote paths short for Windows PowerShell 5.1 and SCP compatibility.
    stage_name = 'cu-host-' + uuid.uuid4().hex[:12]
    windows = node['platform'] == 'windows'
    cache = 'AppData/Local/Cache/distributed-workbench' if windows else '.cache/distributed-workbench'
    staging = cache + '/' + stage_name
    ssh = ['ssh', '-o', 'BatchMode=yes', '-o', 'ClearAllForwardings=yes', alias]
    def remote(command):
        if windows:
            encoded = base64.b64encode(command.encode('utf-16le')).decode()
            return run(ssh + ['powershell.exe -NoProfile -NonInteractive -EncodedCommand ' + encoded])
        return run(ssh + [command])
    expected_environment = environment_content(node)
    if verify:
        if expected_environment is not None:
            encoded = base64.b64encode(expected_environment.encode()).decode()
            if windows:
                quoted_state = "'" + state.replace("'", "''") + "'"
                command = ("$ErrorActionPreference='Stop'; $path=Join-Path " + quoted_state +
                           " 'environment.json'; if (!(Test-Path $path)) { exit 44 }; "
                           "$actual=[Convert]::ToBase64String([IO.File]::ReadAllBytes($path)); "
                           f"if ($actual -ne '{encoded}') {{ exit 45 }}")
            else:
                path = shlex.quote(str(Path(state) / 'environment.json'))
                command = (f"test -f {path} || exit 44; "
                           f"actual=$(base64 < {path} | tr -d '\\n'); "
                           f"test \"$actual\" = {shlex.quote(encoded)} || exit 45")
            try:
                remote(command)
            except subprocess.CalledProcessError as error:
                raise RuntimeError(f"CU environment drift on {executor} (exit {error.returncode})") from error
        print(json.dumps({'node':node['id'], 'hostArtifactDigest':selected,
                          'computerUseEnvironment':desired_environment(node)}))
        return
    with tempfile.TemporaryDirectory(prefix='cu-host-stage-') as directory:
        stage = Path(directory) / stage_name
        (stage / 'scripts').mkdir(parents=True)
        (stage / 'computer-use').mkdir()
        for source, destination in [
            (ROOT / 'scripts/install-computer-use-host.mjs', stage / 'scripts/install.mjs'),
            (ROOT / 'computer-use/release.mjs', stage / 'computer-use/release.mjs'),
            (artifact, stage / 'artifact.json'),
        ]:
            destination.write_bytes(source.read_bytes())
        remote(f"New-Item -ItemType Directory -Force '{cache}' | Out-Null" if windows else f'mkdir -p {cache}')
        run(['scp', '-q', '-r', str(stage), alias + ':' + cache + '/'])
        try:
            if windows:
                quoted_state = "'" + state.replace("'", "''") + "'"
                remote("$ErrorActionPreference='Stop'; $state=" + quoted_state + "; "
                       "$runtime=(Get-Content -Raw (Join-Path $state 'runtime-root')).Trim(); "
                       "$data=Split-Path (Split-Path $runtime -Parent) -Parent; "
                       f"& (Join-Path $runtime 'node.exe') '{staging}/scripts/install.mjs' '{staging}/artifact.json' '{digest}' $state $data; "
                       "if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }")
            else:
                remote('set -eu; state=' + shlex.quote(state) + '; runtime=$(cat "$state/runtime-root"); '
                       'data=$(dirname "$(dirname "$runtime")"); '
                       f'"$runtime/node" {staging}/scripts/install.mjs {staging}/artifact.json {digest} "$state" "$data"')
            if expected_environment is not None:
                encoded = base64.b64encode(expected_environment.encode()).decode()
                if windows:
                    quoted_state = "'" + state.replace("'", "''") + "'"
                    remote("$ErrorActionPreference='Stop'; $state=" + quoted_state + "; "
                           "$path=Join-Path $state 'environment.json'; "
                           "$tmp=$path+'.tmp-'+[guid]::NewGuid().ToString('N'); "
                           f"[IO.File]::WriteAllBytes($tmp,[Convert]::FromBase64String('{encoded}')); "
                           "Move-Item -Force $tmp $path")
                else:
                    path = str(Path(state) / 'environment.json')
                    temporary = path + '.tmp-' + uuid.uuid4().hex
                    remote(f"printf %s {shlex.quote(encoded)} | base64 -d > {shlex.quote(temporary)}; "
                           f"mv {shlex.quote(temporary)} {shlex.quote(path)}")
        finally:
            remote(f"Remove-Item -Recurse -Force '{staging}'" if windows else f'rm -rf {staging}')

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--file', required=True)
    source = parser.add_mutually_exclusive_group()
    source.add_argument('--artifact', type=Path)
    source.add_argument('--url')
    parser.add_argument('--verify-only', action='store_true')
    parser.add_argument('--sha256', required=True)
    args = parser.parse_args()
    if not args.verify_only and args.url:
        cache = Path(os.environ.get('XDG_CACHE_HOME', Path.home() / '.cache')) / 'distributed-workbench' / 'cu-host-artifacts'
        cache.mkdir(parents=True, exist_ok=True)
        if len(args.sha256) != 64 or any(c not in '0123456789abcdef' for c in args.sha256):
            raise ValueError('invalid artifact digest')
        args.artifact = cache / (args.sha256 + '.json')
        if not args.artifact.exists():
            with urllib.request.urlopen(args.url, timeout=60) as response:
                content = response.read(2 * 1024 * 1024 + 1)
            if len(content) > 2 * 1024 * 1024 or hashlib.sha256(content).hexdigest() != args.sha256:
                raise ValueError('invalid host artifact download')
            args.artifact.write_bytes(content)
    if not args.verify_only and (args.artifact is None or hashlib.sha256(args.artifact.read_bytes()).hexdigest() != args.sha256):
        raise ValueError('CU host artifact SHA-256 mismatch')
    binary = os.environ.get('DISTRIBUTED_WORKBENCH_BINARY', 'workbench')
    manifest = json.loads(subprocess.check_output([binary, 'fabric', 'validate', '--file', args.file], text=True))
    for node in manifest['nodes']:
        if args.verify_only:
            deploy_node(binary, node, manifest['initiatorNode'], None, args.sha256, True)
            continue
        executor = node['id'] + ('-native' if node['platform'] == 'windows' else '-rust')
        owner = 'host-upgrade-' + uuid.uuid4().hex
        def maintenance(enabled):
            reply = json.loads(subprocess.check_output([binary, 'call', 'desktop.maintenance', json.dumps({
                'executorId':executor, 'owner':owner, 'enabled':enabled})], text=True))
            if not reply.get('ok'):
                raise RuntimeError(f'desktop maintenance failed: {reply}')
            return reply['result']
        maintenance(True)
        try:
            deadline = time.monotonic() + 120
            while not maintenance(True)['safePoint']:
                if time.monotonic() >= deadline:
                    raise RuntimeError(f'{executor} did not drain; no host selection changed')
                time.sleep(1)
            deploy_node(binary, node, manifest['initiatorNode'], args.artifact.resolve(), args.sha256)
        finally:
            maintenance(False)

if __name__ == '__main__':
    main()
