#!/usr/bin/env python3
"""Stage a checked host-only artifact on declared Fabric nodes, without service restarts."""
import argparse
import base64
import hashlib
import json
import os
import re
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
POWERSHELL_STDIN = '& ([scriptblock]::Create([Text.Encoding]::Unicode.GetString([Convert]::FromBase64String([Console]::In.ReadToEnd()))))'

def run(command, **kwargs):
    return subprocess.run(command, check=True, text=True, **kwargs)

def desired_environment(node):
    desktop = desired_desktop(node)
    if 'computerUseEnvironment' not in node and not (desktop and desktop['enabled']):
        return None
    environment = dict(node.get('computerUseEnvironment') or {})
    if not isinstance(environment, dict) or set(environment) - COMPUTER_USE_ENVIRONMENT_KEYS:
        raise ValueError(f"invalid computerUseEnvironment on {node['id']}")
    if any(not isinstance(value, str) for value in environment.values()):
        raise ValueError(f"computerUseEnvironment values must be strings on {node['id']}")
    headless = environment.get('PI_COMPUTER_USE_HEADLESS')
    if headless is not None and headless not in {'true', 'false'}:
        raise ValueError(f"PI_COMPUTER_USE_HEADLESS must be 'true' or 'false' on {node['id']}")
    if desktop and desktop['enabled']:
        if headless == 'true':
            raise ValueError('windowsDesktop requires headless=false')
        environment['PI_COMPUTER_USE_HEADLESS'] = 'false'
    return environment

def desired_desktop(node):
    policy = node.get('windowsDesktop')
    if policy is None:
        return None
    if (node.get('platform') != 'windows' or not isinstance(policy, dict)
            or set(policy) - {'enabled', 'user'} or type(policy.get('enabled')) is not bool):
        raise ValueError('windowsDesktop requires a Windows node and boolean enabled')
    user = policy.get('user')
    if policy['enabled'] and (not isinstance(user, str) or not user.strip()):
        raise ValueError('windowsDesktop requires an explicit user')
    if not re.fullmatch(r'[A-Za-z0-9._-]+', node['id']):
        raise ValueError('invalid desktop node id')
    return policy

def desktop_command(node, verify=False):
    policy = desired_desktop(node)
    if policy is None:
        return None
    # Use the same deterministic deployment transport as the CU host. No new
    # remote service, password or product-specific state is introduced.
    runtime = base64.b64encode((ROOT / 'scripts/windows-cu-console.ps1').read_bytes()).decode()
    installer = (ROOT / 'scripts/install-windows-desktop.ps1').read_text()
    quote = lambda value: "'" + value.replace("'", "''") + "'"
    command = '& {\n' + installer + '\n} -NodeId ' + quote(node['id'])
    command += ' -RuntimeBase64 ' + quote(runtime)
    if policy['enabled']:
        command += ' -User ' + quote(policy['user'])
    else:
        command += ' -Disable'
    if verify:
        command += ' -VerifyOnly'
    return command

def powershell_node(node, initiator, command):
    encoded = base64.b64encode(command.encode('utf-16le')).decode()
    if node['id'] == initiator:
        return run(['powershell.exe', '-NoProfile', '-NonInteractive', '-Command', POWERSHELL_STDIN], input=encoded)
    alias = node['connection']['sshAlias']
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._-]*', alias):
        raise ValueError('invalid SSH alias')
    return run(['ssh', '-o', 'BatchMode=yes', '-o', 'ClearAllForwardings=yes', alias,
                'powershell.exe -NoProfile -NonInteractive -Command "' + POWERSHELL_STDIN + '"'], input=encoded)

def deploy_desktop(binary, node, initiator, verify=False):
    command = desktop_command(node, verify)
    if command is None:
        return
    executor = node['id'] + '-native'
    reply = json.loads(subprocess.check_output([binary, 'call', 'executor.call', json.dumps({
        'executorId':executor, 'action':'status', 'params':{}})], text=True))
    state = reply.get('result', {}).get('computerUseStateRoot')
    if not reply.get('ok') or not state:
        raise RuntimeError('Windows CU Executor is not ready')
    if desired_desktop(node)['enabled']:
        desired_environment(node)  # Reject conflicting headless policy first.
        quoted = "'" + state.replace("'", "''") + "'"
        environment = "$ErrorActionPreference='Stop'; $path=Join-Path " + quoted + " 'environment.json'; "
        environment += "$envMap=@{}; if(Test-Path $path){$v=Get-Content -Raw $path | ConvertFrom-Json; $v.PSObject.Properties | ForEach-Object {$envMap[$_.Name]=$_.Value}}; "
        if verify:
            environment += "if($envMap['PI_COMPUTER_USE_HEADLESS'] -ne 'false'){throw 'CU headless policy drift'}; "
        else:
            environment += "$envMap['PI_COMPUTER_USE_HEADLESS']='false'; $tmp=$path+'.tmp-'+[guid]::NewGuid().ToString('N'); [IO.File]::WriteAllText($tmp,($envMap|ConvertTo-Json -Compress)); Move-Item -Force $tmp $path; "
        # Apply the environment only after the installer succeeds (bad accounts
        # must not change the host). Verify it before reporting configuration.
        if verify:
            powershell_node(node, initiator, environment)
            powershell_node(node, initiator, command)
        else:
            powershell_node(node, initiator, command)
            powershell_node(node, initiator, environment)
    else:
        powershell_node(node, initiator, command)

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
        desktop = desktop_command(node, verify)
        if desktop:
            powershell_node(node, initiator, desktop)
        if verify:
            local_environment(state, node, True)
            print(json.dumps({'node':node['id'], 'hostArtifactDigest':selected,
                              'computerUseEnvironment':desired_environment(node)}))
            return
        runtime = Path((Path(state) / 'runtime-root').read_text().strip())
        node_path = runtime / 'node-path'
        node_executable = node_path.read_text().strip() if node_path.exists() else str(runtime / 'node')
        if not Path(node_executable).is_absolute():
            raise RuntimeError('computer-use node-path must be absolute')
        run([node_executable, str(ROOT / 'scripts/install-computer-use-host.mjs'),
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
            # The installer is larger than Windows' command-line limit. Send
            # ASCII payload on stdin, never interpolate its contents in a shell.
            return powershell_node(node, initiator, command)
        return run(ssh + [command])
    expected_environment = environment_content(node)
    desktop = desktop_command(node, verify)
    if desktop:
        remote(desktop)
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
                       "$nodeFile=Join-Path $runtime 'node-path'; $nodeExe=Join-Path $runtime 'node.exe'; "
                       "if (Test-Path $nodeFile) { $nodeExe=(Get-Content -Raw $nodeFile).Trim() }; "
                       f"& $nodeExe '{staging}/scripts/install.mjs' '{staging}/artifact.json' '{digest}' $state $data; "
                       "if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }")
            else:
                remote('set -eu; state=' + shlex.quote(state) + '; runtime=$(cat "$state/runtime-root"); '
                       'data=$(dirname "$(dirname "$runtime")"); '
                       'node_bin="$runtime/node"; if [ -f "$runtime/node-path" ]; then node_bin=$(cat "$runtime/node-path"); fi; '
                       f'"$node_bin" {staging}/scripts/install.mjs {staging}/artifact.json {digest} "$state" "$data"')
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
    target = parser.add_mutually_exclusive_group(required=True)
    target.add_argument('--file')
    target.add_argument('--desktop-node-json', help='One already registered Windows node; policy-only reconciliation')
    parser.add_argument('--local-node', action='store_true')
    source = parser.add_mutually_exclusive_group()
    source.add_argument('--artifact', type=Path)
    source.add_argument('--url')
    parser.add_argument('--verify-only', action='store_true')
    parser.add_argument('--sha256')
    args = parser.parse_args()
    binary = os.environ.get('DISTRIBUTED_WORKBENCH_BINARY', 'workbench')
    desktop_only = args.desktop_node_json is not None
    if desktop_only:
        node = json.loads(args.desktop_node_json)
        if desired_desktop(node) is None:
            parser.error('desktop-node-json needs windowsDesktop')
        desired_environment(node)
        manifest = {'nodes':[node], 'initiatorNode':node['id'] if args.local_node else ''}
    elif not args.sha256:
        parser.error('--sha256 is required for host installation')
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
    if not desktop_only and not args.verify_only and (args.artifact is None or hashlib.sha256(args.artifact.read_bytes()).hexdigest() != args.sha256):
        raise ValueError('CU host artifact SHA-256 mismatch')
    if not desktop_only:
        manifest = json.loads(subprocess.check_output([binary, 'fabric', 'validate', '--file', args.file], text=True))
    # Validate every desktop policy before the first node mutation.
    for node in manifest['nodes']:
        desired_environment(node)
    for node in manifest['nodes']:
        if args.verify_only:
            if desktop_only:
                deploy_desktop(binary, node, manifest['initiatorNode'], True)
            else:
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
            if desktop_only:
                deploy_desktop(binary, node, manifest['initiatorNode'])
            else:
                deploy_node(binary, node, manifest['initiatorNode'], args.artifact.resolve(), args.sha256)
        finally:
            maintenance(False)

if __name__ == '__main__':
    main()
