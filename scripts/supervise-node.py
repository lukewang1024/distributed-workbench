#!/usr/bin/env python3
"""Idempotent headless node supervision using a private supervisord instance.

The caller owns installation, wake hooks and peer topology. No system manager,
desktop runtime, elevated privileges or provider-specific behavior is assumed.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import socket
import subprocess
import tempfile
import time


def validate(config):
    required = {"nodeId", "binary", "supervisor", "stateRoot", "allowRoots"}
    if set(config) - {"peers"} != required:
        raise ValueError("node configuration requires exactly: " + ", ".join(sorted(required)))
    if not isinstance(config["nodeId"], str) or not re.fullmatch(r"[A-Za-z0-9._-]+", config["nodeId"]):
        raise ValueError("invalid nodeId")
    if not isinstance(config["allowRoots"], list) or not config["allowRoots"]:
        raise ValueError("allowRoots must be a nonempty list")
    for value in [config[k] for k in ("binary", "supervisor", "stateRoot")] + config["allowRoots"]:
        if not isinstance(value, str) or not value.startswith("/") or any(c in value for c in "\n\r\0%"):
            raise ValueError("paths must be absolute and contain no newline, NUL or percent")
    if len(os.fsencode(config["stateRoot"] + "/supervisor.sock")) >= 104:
        raise ValueError("stateRoot is too long for a Unix socket")
    peers = config.get("peers", [])
    if not isinstance(peers, list):
        raise ValueError("peers must be a list")
    identities = {config["nodeId"]}
    for peer in peers:
        if set(peer) != {"id", "binary", "stateRoot", "sshAlias"}:
            raise ValueError("invalid peer fields")
        for key in ("id", "sshAlias"):
            if not re.fullmatch(r"[A-Za-z0-9._-]+", peer[key]):
                raise ValueError("invalid peer identity or alias")
        if peer["id"] in identities:
            raise ValueError("duplicate peer identity")
        identities.add(peer["id"])
        for key in ("binary", "stateRoot"):
            if not peer[key].startswith("/") or any(c in peer[key] for c in "\r\n\0%"):
                raise ValueError("invalid peer path")
        for path in (config["stateRoot"] + "/peers/" + peer["id"] + "/controller.sock",
                     peer["stateRoot"] + "/fabric/" + config["nodeId"] + "-controller.sock"):
            if len(os.fsencode(path)) >= 104:
                raise ValueError("peer socket path is too long")
    return config


def atomic_write(path, content, mode=0o600):
    data = content.encode()
    if path.exists() and path.read_bytes() == data and path.stat().st_mode & 0o777 == mode:
        return False
    fd, name = tempfile.mkstemp(prefix=".reconcile-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as handle:
            os.fchmod(handle.fileno(), mode)
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(name, path)
    finally:
        if os.path.exists(name):
            os.unlink(name)
    return True


class Node:
    def __init__(self, config):
        self.config = validate(config)
        self.root = Path(config["stateRoot"])
        self.ini = self.root / "supervisord.conf"

    def run(self, argv):
        return subprocess.check_output(argv, text=True, stderr=subprocess.PIPE, timeout=15).strip()

    def ctl(self, *argv):
        return self.run([self.config["supervisor"], "-c", str(self.ini), "ctl", *argv])

    def listening(self):
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(1)
            try:
                client.connect(str(self.root / "supervisor.sock"))
                return True
            except (FileNotFoundError, ConnectionRefusedError):
                return False

    def commands(self):
        c = self.config
        commands = {
            "controller": [c["binary"], "--socket", str(self.root / "controller.sock"),
                           "controller", "serve", "--state", str(self.root / "controller.json"), "--id", c["nodeId"]],
            "executor": [c["binary"], "--socket", str(self.root / "executor.sock"),
                         "executor", "serve", "--id", c["nodeId"] + "-rust"] +
                        [arg for root in c["allowRoots"] for arg in ("--allow-root", root)],
        }
        for peer in c.get("peers", []):
            root = self.root / "peers" / peer["id"]
            commands["peer-" + peer["id"]] = [c["binary"], "peer", "connect", "--id", peer["id"],
                "--local-id", c["nodeId"], "--host", peer["sshAlias"],
                "--local-controller-socket", str(self.root / "controller.sock"),
                "--local-executor-socket", str(self.root / "executor.sock"),
                "--expose-controller-socket", str(root / "controller.sock"),
                "--expose-executor-socket", str(root / "executor.sock"),
                "--remote-executable", peer["binary"], "--remote-state-root", peer["stateRoot"],
                "--state", str(root / "status.json")]
        return commands

    def desired(self):
        result = {}
        ini = ("[unix_http_server]\nfile=" + str(self.root / "supervisor.sock") + "\nchmod=0600\n"
               "[supervisord]\nlogfile=" + str(self.root / "supervisor.log") + "\n"
               "pidfile=" + str(self.root / "supervisor.pid") + "\n"
               "[supervisorctl]\nserverurl=unix://" + str(self.root / "supervisor.sock") + "\n"
               "[rpcinterface:supervisor]\nsupervisor.rpcinterface_factory=supervisor.rpcinterface:make_main_rpcinterface\n")
        for role, argv in self.commands().items():
            wrapper = self.root / (role + ".sh")
            result[wrapper] = ("#!/bin/sh\nset -eu\nexec " + shlex.join(argv) + "\n", 0o700)
            ini += ("\n[program:" + role + "]\ncommand=/bin/sh " + shlex.quote(str(wrapper)) +
                    "\nautostart=true\nautorestart=true\nstartsecs=1\nstopsignal=TERM\nstopwaitsecs=10\n"
                    "stopasgroup=true\nkillasgroup=true\nstdout_logfile=" + str(self.root / (role + ".log")) +
                    "\nstderr_logfile=" + str(self.root / (role + "-error.log")) + "\n")
        result[self.ini] = (ini, 0o600)
        digest = hashlib.sha256(Path(self.config["binary"]).read_bytes()).hexdigest()
        identity = {"config": self.config, "binarySha256": digest}
        result[self.root / "installed.json"] = (json.dumps(identity, sort_keys=True, indent=2) + "\n", 0o600)
        return result

    def rpc(self, role, method="status", params=None):
        return self.rpc_at(self.root / (role + ".sock"), method, params)

    def rpc_at(self, endpoint, method="status", params=None):
        argv = [self.config["binary"], "--socket", str(endpoint)]
        argv += ["status"] if method == "status" else ["call", method, json.dumps(params)]
        reply = json.loads(self.run(argv))
        if not reply.get("ok"):
            raise RuntimeError("RPC failed: " + str(endpoint) + " " + method)
        return reply["result"]

    def peer_status(self, mutate=False):
        results = []
        for peer in self.config.get("peers", []):
            root = self.root / "peers" / peer["id"]
            state = json.loads((root / "status.json").read_text())
            if state.get("state") != "ready" or state.get("peerId") != peer["id"]:
                raise RuntimeError("peer is not ready: " + peer["id"])
            remote = root / "controller.sock"
            for owner_socket, expected_owner, other, base, reverse in (
                    (self.root / "controller.sock", self.config["nodeId"], peer["id"], root, False),
                    (remote, peer["id"], self.config["nodeId"], Path(peer["stateRoot"]) / "fabric", True)):
                status = self.rpc_at(owner_socket)
                if status.get("controller", {}).get("id") != expected_owner:
                    raise RuntimeError("peer Controller identity mismatch")
                for role in ("controller", "executor"):
                    identity = other + ("-rust" if role == "executor" else "")
                    endpoint = {"transport": "local", "socket": str(base / ((other + "-" if reverse else "") + role + ".sock"))}
                    registered = next((r for r in status.get(role + "s", []) if r["id"] == identity), None)
                    if registered and registered.get("endpoint") != endpoint:
                        raise RuntimeError("registration conflict: " + identity)
                    if not registered:
                        if not mutate:
                            raise RuntimeError("missing peer registration: " + identity)
                        self.rpc_at(owner_socket, role + ".register", {role + "Id": identity, "endpoint": endpoint})
            reverse_status = self.rpc_at(remote, "controller.call", {
                "controllerId": self.config["nodeId"], "action": "status", "params": {}})
            if reverse_status.get("controller", {}).get("id") != self.config["nodeId"]:
                raise RuntimeError("reverse Controller identity mismatch")
            results.append({"id": peer["id"], "generation": state["generation"], "bidirectional": True})
        return results

    def status(self):
        if not self.listening():
            raise RuntimeError("supervisor is not listening")
        pids = {role: int(self.ctl("pid", role)) for role in self.commands()}
        if any(pid <= 0 for pid in pids.values()):
            raise RuntimeError("supervisor has a missing child")
        for pid in pids.values():
            os.kill(pid, 0)
        controller = self.rpc("controller")
        executor = self.rpc("executor")
        if controller.get("controller", {}).get("id") != self.config["nodeId"]:
            raise RuntimeError("controller identity mismatch")
        executor_id = self.config["nodeId"] + "-rust"
        if executor.get("executorId") != executor_id:
            raise RuntimeError("executor identity mismatch")
        if set(executor.get("configuredRoots", [])) != set(self.config["allowRoots"]):
            raise RuntimeError("running executor roots differ from desired input")
        endpoint = {"transport": "local", "socket": str(self.root / "executor.sock")}
        registered = any(e["id"] == executor_id and e.get("endpoint") == endpoint
                         and e.get("health") == "ready" for e in controller.get("executors", []))
        return {"pids": pids, "registered": registered, "nodeId": self.config["nodeId"]}

    def reconcile(self, mode):
        if mode == "status":
            status = self.status()
            if not status["registered"]:
                raise RuntimeError("local executor is not registered and ready")
            return dict(status, peers=self.peer_status())
        desired = self.desired()
        changes = [str(p) for p, (s, m) in desired.items()
                   if not p.exists() or p.read_text() != s or p.stat().st_mode & 0o777 != m]
        if mode == "plan":
            return {"nodeId": self.config["nodeId"], "changedFiles": changes}
        if mode == "stop":
            if self.listening():
                # Some supervisor implementations kill children immediately on
                # shutdown. Explicit stop honors each program's stop policy.
                for role in reversed(list(self.commands())):
                    if int(self.ctl("pid", role)) > 0:
                        self.ctl("stop", role)
                self.ctl("shutdown")
                deadline = time.monotonic() + 30
                while self.listening():
                    if time.monotonic() >= deadline:
                        raise RuntimeError("supervisor did not stop")
                    time.sleep(.1)
            return {"stopped": True}
        if changes and self.listening():
            raise RuntimeError("running node configuration changed; drain and stop the node before applying")
        for path, (content, mode_bits) in desired.items():
            atomic_write(path, content, mode_bits)
        if not self.listening():
            self.run([self.config["supervisor"], "-c", str(self.ini), "-d"])
        deadline = time.monotonic() + 30
        while True:
            try:
                status = self.status()
                if not status["registered"]:
                    self.rpc("controller", "executor.register", {"executorId": self.config["nodeId"] + "-rust",
                        "endpoint": {"transport": "local", "socket": str(self.root / "executor.sock")}})
                    status = self.status()
                if status["registered"]:
                    return dict(status, changedFiles=changes, peers=self.peer_status(mutate=True))
            except (RuntimeError, ValueError, OSError, subprocess.SubprocessError):
                if time.monotonic() >= deadline:
                    raise
            if time.monotonic() >= deadline:
                raise RuntimeError("local executor did not become ready")
            time.sleep(.2)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--file", required=True)
    parser.add_argument("mode", choices=["plan", "ensure", "status", "stop"])
    args = parser.parse_args()
    node = Node(json.loads(Path(args.file).read_text()))
    if args.mode in {"plan", "status"}:
        result = node.reconcile(args.mode)
    else:
        node.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        if node.root.stat().st_uid != os.getuid():
            raise RuntimeError("run as the node state directory owner")
        with (node.root / "reconcile.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            result = node.reconcile(args.mode)
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
