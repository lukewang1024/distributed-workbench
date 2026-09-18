#!/usr/bin/env python3
"""Make one explicit Controller call through a configured gateway; never replay."""
import argparse
import json
from pathlib import Path
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--node-state", required=True, help="installed.json from the headless node")
    parser.add_argument("--gateway", help="configured peer ID; optional when exactly one exists")
    parser.add_argument("action")
    parser.add_argument("params", help="JSON params, or - to read stdin")
    args = parser.parse_args()
    config = json.loads(Path(args.node_state).read_text())["config"]
    peers = [p["id"] for p in config.get("peers", [])]
    gateway = args.gateway or (peers[0] if len(peers) == 1 else None)
    if gateway not in peers:
        parser.error("select one configured gateway with --gateway")
    params = json.loads(sys.stdin.read() if args.params == "-" else args.params)
    command = [config["binary"], "--socket", config["stateRoot"] + "/controller.sock", "call", "controller.call",
               json.dumps({"controllerId": gateway, "action": args.action, "params": params})]
    return subprocess.run(command).returncode


if __name__ == "__main__":
    sys.exit(main())
