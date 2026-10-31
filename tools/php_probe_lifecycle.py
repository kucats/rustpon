#!/usr/bin/env python3
"""Native PHP 8.4 extension request-lifecycle smoke test, single worker."""
import argparse
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time

BODY = (b'--b\r\nContent-Disposition: form-data; name="up"; filename="x.bin"\r\n'
        b'Content-Type: application/octet-stream\r\n\r\nABC\x00XYZ\r\n--b--\r\n')


def request(port, method, path, body=b""):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        conn.request(method, path, body=body, headers={
            "Content-Type": "application/octet-stream", "Connection": "close"
        })
        response = conn.getresponse()
        payload = response.read()
        return response.status, payload
    finally:
        conn.close()


def fd_count(pid):
    return len(os.listdir(f"/proc/{pid}/fd"))


def eventually_baseline(pid, baseline):
    for _ in range(80):
        if fd_count(pid) == baseline:
            return
        time.sleep(0.025)
    raise AssertionError(f"PHP worker FD leak: initial {baseline}, current {fd_count(pid)}")


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--module", type=Path, required=True)
    p.add_argument("--library", type=Path, required=True)
    p.add_argument("--php-bin", default="php")
    args = p.parse_args()
    module = args.module.resolve()
    library = args.library.resolve()
    assert module.is_file() and library.is_file(), (module, library)
    router = Path(__file__).resolve().parents[1] / "ffi" / "php_probe" / "test_router.php"

    with tempfile.TemporaryDirectory(prefix="rustpon-php-shutdown-") as private_dir:
        os.chmod(private_dir, 0o700)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        env = os.environ.copy()
        env["LD_LIBRARY_PATH"] = str(library.parent) + ":" + env.get("LD_LIBRARY_PATH", "")
        proc = subprocess.Popen(
            [
                args.php_bin, "-n", "-d", f"extension={module}",
                "-d", f"rustpon_probe.spool_dir={private_dir}",
                "-S", f"127.0.0.1:{port}", str(router),
            ],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE, env=env,
        )
        try:
            for _ in range(100):
                try:
                    status, content = request(port, "GET", "/")
                    if status == 200:
                        break
                except OSError:
                    if proc.poll() is not None:
                        raise RuntimeError(f"PHP exited: {proc.stderr.read()!r}")
                    time.sleep(0.05)
            else:
                raise RuntimeError("PHP probe server not started")
            state = json.loads(content)
            assert state["probe"] and state["functions"], state
            pid = state["pid"]
            assert pid == proc.pid, (pid, proc.pid)
            baseline = fd_count(pid)
            for i in range(32):
                mode = ("normal", "exit", "multi", "uncaught")[i % 4]
                status, content = request(port, "POST", f"/?mode={mode}", BODY)
                if mode == "uncaught":
                    assert status == 500, (i, mode, status, content)
                else:
                    assert status == 200, (i, mode, status, content)
                    data = json.loads(content)
                    if mode != "exit":
                        assert data == {"event": "ok", "bytes": 7, "typed": True}, (i, data)
                eventually_baseline(pid, baseline)
                assert list(Path(private_dir).iterdir()) == [], f"Spool leak on {i}"
                status, body = request(port, "GET", "/")
                assert status == 200 and json.loads(body)["pid"] == pid
                eventually_baseline(pid, baseline)
            print("PHP 8.4 native extension: 32 HTTP requests (normal/exit/multi/uncaught), "
                  "single worker reused, RSHUTDOWN FD cleanup confirmed")
        finally:
            proc.terminate()
            try:
                _, stderr = proc.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                _, stderr = proc.communicate()
            if proc.returncode not in (0, -15):
                print("PHP worker stderr tail:", stderr[-4000:].decode(errors="replace"))


if __name__ == "__main__":
    main()
