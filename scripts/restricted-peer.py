#!/usr/bin/env python3
"""An SSH forced command bound to one peer identity and existing local node.

Install using an authorized_keys restrict,command= entry. No shell command from
the client is executed. Only the exact peer accept vector and --version pass.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shlex
import sys


def allowed(config):
    if set(config) != {"binary", "nodeId", "peerId", "stateRoot"}:
        raise ValueError("invalid restricted peer config")
    for key in ("nodeId", "peerId"):
        if not re.fullmatch(r"[A-Za-z0-9._-]+", config[key]):
            raise ValueError("invalid peer identity")
    for key in ("binary", "stateRoot"):
        if not config[key].startswith("/") or any(c in config[key] for c in "\r\n\0"):
            raise ValueError("invalid absolute path")
    root, peer = config["stateRoot"], config["peerId"]
    return [config["binary"], "peer", "accept", "--id", peer, "--local-id", config["nodeId"],
            "--local-controller-socket", root + "/controller.sock",
            "--local-executor-socket", root + "/executor.sock",
            "--expose-controller-socket", root + "/fabric/" + peer + "-controller.sock",
            "--expose-executor-socket", root + "/fabric/" + peer + "-executor.sock"]


def authorize(config, command):
    expected = allowed(config)
    requested = shlex.split(command)
    if requested not in (expected, [config["binary"], "--version"]):
        raise ValueError("SSH command is outside this peer identity's scope")
    return requested


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--file", required=True)
    args = parser.parse_args()
    config = json.loads(Path(args.file).read_text())
    command = authorize(config, os.environ.get("SSH_ORIGINAL_COMMAND", ""))
    os.execv(command[0], command)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError) as error:
        print("restricted peer: " + str(error), file=sys.stderr)
        sys.exit(126)
