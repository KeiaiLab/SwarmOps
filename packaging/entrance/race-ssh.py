#!/usr/bin/python3
"""ssh for git (GIT_SSH_COMMAND, with GIT_SSH_VARIANT=ssh) that loses a push's race on purpose.

The first `git-receive-pack` connection while RACE_MARKER does not exist creates it and relays
ssh: once the server's advertisement is through and the client starts sending its commands, it
first runs RACE_COMMAND (a JSON list) to its end, which moves the ref the client named, writes
its exit status into RACE_MARKER, then relays the rest. Every other connection is plain ssh.
Exit status: ssh's.

    client ──▶ race-ssh ──▶ ssh ──▶ sshd ──▶ swarmops serve
                  │ first client bytes held
                  └─ RACE_COMMAND runs: another push lands meanwhile
"""

import json
import os
import subprocess
import sys
import threading

CHUNK = 65536


def pump(source: int, sink: int) -> None:
    """Copies `source` to `sink` until end of file."""
    while chunk := os.read(source, CHUNK):
        os.write(sink, chunk)


def main() -> int:
    args = sys.argv[1:]
    marker = os.environ.get("RACE_MARKER", "")
    pushing = any("git-receive-pack" in arg for arg in args)

    # 1. Not the connection to race: plain ssh.
    if not marker or not pushing or os.path.exists(marker):
        os.execvp("ssh", ["ssh", *args])
    with open(marker, "x", encoding="utf-8"):
        pass

    # 2. The server's side flows at once; the client's first bytes wait for the race.
    child = subprocess.Popen(["ssh", *args], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    assert child.stdin is not None and child.stdout is not None, "pipes requested"
    down = threading.Thread(target=pump, args=(child.stdout.fileno(), sys.stdout.fileno()))
    down.start()
    first = os.read(sys.stdin.fileno(), CHUNK)
    race = subprocess.run(
        json.loads(os.environ["RACE_COMMAND"]),
        check=False,
        env=race_env(),
        stdin=subprocess.DEVNULL,
        stdout=sys.stderr,
    )
    with open(marker, "w", encoding="utf-8") as told:
        told.write(f"{race.returncode}\n")

    # 3. The rest of the client's push, then ssh's end.
    os.write(child.stdin.fileno(), first)
    pump(sys.stdin.fileno(), child.stdin.fileno())
    child.stdin.close()
    down.join()
    return child.wait()


def race_env() -> dict[str, str]:
    """This environment without the relay, so the race command's own git uses plain ssh."""
    env = dict(os.environ)
    for name in ("GIT_SSH_COMMAND", "GIT_SSH_VARIANT", "RACE_MARKER", "RACE_COMMAND"):
        env.pop(name, None)
    return env


if __name__ == "__main__":
    sys.exit(main())
