#!/usr/bin/env python3
"""Exercise three real Controllers competing for two durable Executor desktops."""
import concurrent.futures
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

root = Path(__file__).resolve().parent.parent
binary = Path(os.environ.get("WORKBENCH_TEST_BINARY", root / "target/debug/workbench"))
with tempfile.TemporaryDirectory(prefix="desktop-fifo-") as directory:
    base = Path(directory)
    children = []
    def start(role, name):
        args = [str(binary), role, "serve", "--id", name, "--socket", str(base / (name + ".sock")), "--state", str(base / name / "state.json")]
        if role == "executor":
            args += ["--allow-root", directory]
        child = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        children.append(child)
        for _ in range(200):
            if (base / (name + ".sock")).exists():
                return child
            time.sleep(.025)
        raise AssertionError("server did not start")
    def rpc(controller, action, params, ok=True):
        result = subprocess.run([str(binary), "--socket", str(base / (controller + ".sock")), "call", action, json.dumps(params)], capture_output=True, text=True, timeout=30)
        value = json.loads(result.stdout)
        if ok:
            assert value["ok"], value
            return value["result"]
        assert not value["ok"], value
        return value["error"]["code"]
    def creds(job, executor="desktop"):
        return {"executorId": executor, "owner": job["owner"], "token": job["token"]}
    def submit(c, owner, executor="desktop", ttl=10000):
        return rpc(c,"desktop.submit",{"executorId":executor,"owner":owner,"requestKey":owner,"ttlMs":ttl})
    try:
        for c in ("a", "b", "c"):
            start("controller", c)
        desktop = start("executor", "desktop")
        start("executor", "independent")
        for c in ("a", "b", "c"):
            for e in ("desktop", "independent"):
                rpc(c, "executor.register", {"executorId":e,"endpoint":{"transport":"local","socket":str(base / (e + ".sock"))}})
        first = submit("a", "first")
        with concurrent.futures.ThreadPoolExecutor() as pool:
            pending = list(pool.map(lambda c: (c, submit(c, c)), ["b", "c"]))
        assert all(job["state"] == "queued" for _, job in pending)
        assert submit("b", "b")["id"] == pending[0][1]["id"]
        queue = rpc("c","desktop.list",{"executorId":"desktop"})["jobs"]
        order = [job["owner"] for job in queue]
        assert order[0] == "first" and set(order[1:]) == {"b", "c"}
        assert submit("b", "other", "independent")["state"] == "active"
        for action in ("ui.input", "ui.evaluate", "application.launch", "clipboard.write", "computer-use.call"):
            code=rpc("b","executor.call",{"executorId":"desktop","action":action,"params":{}},ok=False)
            assert code == "DESKTOP_BUSY", (action,code)
        rpc("a","desktop.finish",creds(first))
        by_owner = dict(pending)
        for owner in order[1:]:
            job = by_owner[owner]
            assert rpc(owner,"desktop.get",creds(job))["state"] == "active"
            rpc(owner,"desktop.finish",creds(job))
        assert rpc("a","executor.call",{"executorId":"desktop","action":"computer-use.call","params":{"_desktop":creds(first)}},ok=False) == "DESKTOP_SESSION_REQUIRED"
        expiring=submit("a","expiring",ttl=1000)
        cancelled=submit("b","cancelled")
        successor=submit("c","successor")
        assert rpc("b","desktop.cancel",creds(cancelled))["state"] == "cancelled"
        time.sleep(2)
        assert rpc("c","desktop.get",creds(successor))["state"] == "active"
        assert rpc("a","desktop.get",creds(expiring))["state"] == "expired"
        # Restart with ownership outstanding: fail closed, preserve pending work.
        waiting=submit("b","after-restart")
        desktop.terminate(); desktop.wait(timeout=10)
        (base / "desktop.sock").unlink(missing_ok=True)
        start("executor","desktop")
        state=rpc("a","desktop.list",{"executorId":"desktop"})
        assert state["blocked"]
        assert rpc("b","desktop.get",creds(waiting))["state"] == "queued"
        # No native GUI call ran in this test; reset is already confirmed.
        rpc("a","desktop.recover",{"executorId":"desktop","confirmDesktopReset":True})
        assert rpc("b","desktop.get",creds(waiting))["state"] == "active"
        rpc("b","desktop.finish",creds(waiting))
        maintenance = {"executorId":"desktop", "owner":"upgrade", "enabled":True}
        assert rpc("a", "desktop.maintenance", maintenance)["safePoint"]
        pending = submit("b", "during-maintenance")
        assert pending["state"] == "queued"
        assert rpc("c", "desktop.list", {"executorId":"desktop"})["safePoint"]
        assert rpc("c", "desktop.maintenance", {**maintenance, "owner":"other", "enabled":False}, ok=False) == "MAINTENANCE_OWNED"
        rpc("a", "desktop.maintenance", {**maintenance, "enabled":False})
        assert rpc("b", "desktop.get", creds(pending))["state"] == "active"
        rpc("b", "desktop.finish", creds(pending))
        print(json.dumps({"passed":True,"controllers":3,"executors":2,"fifo":order,"checks":["concurrent-submit","dedup","legacy-exclusion","independent-desktops","stale-token","cancel","automatic-expiry","restart-quarantine","recovery","maintenance-admission"]}))
    finally:
        for child in reversed(children):
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    child.kill(); child.wait()
