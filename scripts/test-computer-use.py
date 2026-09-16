#!/usr/bin/env python3
"""Bounded real Controller -> Executor -> upstream Pi extension smoke test."""
import json, os, pathlib, subprocess, tempfile, time
root = pathlib.Path(__file__).resolve().parent.parent
binary = root / 'target/debug/workbench'
with tempfile.TemporaryDirectory(prefix='cu-rpc-') as directory:
    state = pathlib.Path(directory)
    controller, executor = state/'c.sock', state/'e.sock'
    env = dict(os.environ, WORKBENCH_COMPUTER_USE_ROOT=str(root/'computer-use'))
    children = []
    def rpc(action, params):
        p = subprocess.run([str(binary), '--socket', str(controller), 'call', action, json.dumps(params)], capture_output=True, text=True, timeout=120)
        return json.loads(p.stdout)
    try:
        for role, endpoint in [('controller',controller), ('executor',executor)]:
            args = [str(binary), role, 'serve', '--id', role, '--socket', str(endpoint), '--state', str(state/(role+'.json'))]
            if role == 'executor': args += ['--allow-root', directory]
            children.append(subprocess.Popen(args, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        for _ in range(200):
            if controller.exists() and executor.exists(): break
            time.sleep(.025)
        assert rpc('executor.register', {'executorId':'executor','endpoint':{'transport':'local','socket':str(executor)}})['ok']
        discovery = rpc('executor.call', {'executorId':'executor','action':'computer-use.tools','params':{}})
        assert discovery['ok'], discovery
        assert len(discovery['result']['tools']) == 11
        request = {'executorId':'executor','action':'computer-use.call','params':{'sessionId':'test','tool':'search_ui','arguments':{'stateId':'expired','text':'test'}}}
        denied = rpc('executor.call', request)
        assert not denied['ok'] and denied['error']['code'] == 'EXECUTOR_AUTHORITY_REQUIRED', denied
        lease = rpc('lease.acquire', {'resource':'computer-use:executor','owner':'test','ttlMs':300000})['result']
        assert not rpc('lease.acquire', {'resource':'computer-use:executor','owner':'other','ttlMs':300000})['ok']
        request.update(leaseResource='computer-use:executor',owner='test',token=lease['token'])
        stale = rpc('executor.call', request)
        assert not stale['ok'] and 'unavailable or was evicted' in stale['error']['message'], stale
        request['params']['tool'] = 'close'
        request['params']['arguments'] = {}
        assert rpc('executor.call', request)['ok']
        assert rpc('lease.release', {'resource':'computer-use:executor','owner':'test','token':lease['token']})['ok']
        assert not rpc('executor.call', request)['ok']
        print('PASS: original plugin discovery, fenced calls, contention, stale refs, close, stale token rejection')
    finally:
        for child in reversed(children):
            child.terminate()
            try: child.wait(timeout=10)
            except subprocess.TimeoutExpired: child.kill(); child.wait()
