"""Configure the hosted runner's Docker Hub cache before BuildKit bootstrap.

The optional path is for filesystem regression fixtures, not another registry.
Restarting Docker is explicit in the workflow, after successful configuration.
"""
import json
import os
from pathlib import Path
import stat
import sys
import tempfile


def configure(path):
    try:
        config = json.loads(path.read_text())
        mode = stat.S_IMODE(path.stat().st_mode)
    except FileNotFoundError:
        config = {}
        mode = 0o644
    if not isinstance(config, dict):
        raise ValueError("Docker daemon configuration must be a JSON object")
    mirrors = config.get("registry-mirrors", [])
    if not isinstance(mirrors, list) or not all(isinstance(item, str) for item in mirrors):
        raise ValueError("Docker registry-mirrors must be an array of strings")
    mirror = "https://mirror.gcr.io"
    config["registry-mirrors"] = [mirror] + [item for item in mirrors if item.rstrip("/") != mirror]
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, delete=False) as output:
            temporary = Path(output.name)
            json.dump(config, output, indent=2)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        temporary.chmod(mode)
        os.replace(temporary, path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
    print("Docker Hub cache configured: https://mirror.gcr.io (Docker Hub fallback on cache miss)")


if __name__ == "__main__":
    configure(Path(sys.argv[1]) if len(sys.argv) > 1 else Path("/etc/docker/daemon.json"))
