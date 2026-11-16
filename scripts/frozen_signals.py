"""Exercise real CLI OS signals in an isolated console/process group."""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading

from frozen_compare import assert_observation

ROOT = Path(__file__).resolve().parents[1]


def worker(directory, binary, mode):
    if os.name == "nt" and mode == "alternate":
        # This worker owns an isolated hidden console. Keep its Python handlers
        # alive while broadcasting only within that console, shared with CLI.
        signal.signal(signal.SIGINT, lambda *_: None)
        signal.signal(signal.SIGBREAK, lambda *_: None)
        import ctypes
        assert ctypes.windll.kernel32.SetConsoleCtrlHandler(None, False), "cannot enable Ctrl-C in isolated console"
    arrived, cancelling, release = threading.Event(), threading.Event(), threading.Event()
    requests = []

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            requests.append(self.path)
            if self.path == "/cancel":
                cancelling.set()
                if not release.wait(10):
                    return
                body, status = b'{"cancelled":true}', 200
            else:
                body, status = b'{"id":"job-1","status":"queued"}', 202
            self.send_response(status)
            self.send_header("Content-Length", str(len(body)))
            if status == 202:
                self.send_header("Location", "/poll")
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            requests.append(self.path)
            arrived.set()
            release.wait(10)
            # The cancelled request has no response to write.
            self.close_connection = True

        def log_message(self, *args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    request = {"schema_version":1,"operation":"test", "connection":{
        "url":f"http://127.0.0.1:{server.server_port}/submit", "method":"POST",
        "polling":{"status_path":"status","interval_ms":0,"max_attempts":10,
                   "cancel":{"url_template":"/cancel","method":"POST"}}}}
    config = directory / "engine.json"
    config.write_text(json.dumps({"allow_private_networks":True}), encoding="utf-8")
    child = subprocess.Popen([str(binary),"test","--input","-","--config",str(config),"--format","json"],
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" and mode == "primary" else 0,
                             start_new_session=os.name != "nt")
    try:
        child.stdin.write(json.dumps(request).encode())
        child.stdin.close()
        child.stdin = None
        assert arrived.wait(8), "CLI never entered polling"
        first = signal.CTRL_BREAK_EVENT if os.name == "nt" else (signal.SIGINT if mode == "primary" else signal.SIGTERM)
        second = signal.CTRL_BREAK_EVENT if os.name == "nt" else (signal.SIGTERM if mode == "primary" else signal.SIGINT)
        def send(event):
            if os.name == "nt" and mode == "alternate":
                import ctypes
                assert ctypes.windll.kernel32.GenerateConsoleCtrlEvent(event,0), "cannot send isolated console event"
            else:
                child.send_signal(event)
        send(signal.CTRL_C_EVENT if os.name == "nt" and mode == "alternate" else first)
        assert cancelling.wait(3), "first signal did not start remote cancellation"
        send(second)
        try:
            child.wait(timeout=0.2)
            raise AssertionError("second signal truncated cooperative cancellation")
        except subprocess.TimeoutExpired:
            pass
        release.set()
        stdout, stderr = child.communicate(timeout=8)
        assert child.returncode == 130, (child.returncode, stdout, stderr)
        assert stderr == b"", stderr
        document = json.loads(stdout)
        assert document["error"]["code"] == "CANCELLED", document
        assert document["error"]["details"]["async_jobs"][0]["cancel_accepted"] is True, document
        assert requests.count("/cancel") == 1, requests
        result = {"exit":child.returncode,"document":document,"requests":requests}
        (directory / "result.json").write_text(json.dumps(result, indent=2), encoding="utf-8")
    finally:
        release.set()
        if child.poll() is None:
            child.kill()
            child.wait()
        server.shutdown()
        server.server_close()
        thread.join()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--worker", type=Path)
    parser.add_argument("--mode",choices=("primary","alternate","both"),default="both")
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug" / ("plenora-rest.exe" if os.name == "nt" else "plenora-rest"))
    args = parser.parse_args()
    if args.mode == "both":
        results = [subprocess.run([sys.executable,__file__,"--mode",mode,"--binary",str(args.binary)],timeout=35).returncode for mode in ("primary","alternate")]
        return 0 if results == [0,0] else 1
    if args.worker:
        worker(args.worker, args.binary, args.mode)
        return 0
    with tempfile.TemporaryDirectory(prefix="signals-") as temporary:
        directory = Path(temporary)
        options = {}
        if os.name == "nt":
            startup = subprocess.STARTUPINFO()
            startup.dwFlags = subprocess.STARTF_USESHOWWINDOW
            startup.wShowWindow = subprocess.SW_HIDE
            options = {"creationflags":subprocess.CREATE_NEW_CONSOLE,"startupinfo":startup}
        result = subprocess.run([sys.executable, __file__, "--worker",str(directory),"--binary",str(args.binary),"--mode",args.mode], capture_output=True, timeout=30, **options)
        output = ROOT / "target/frozen/signals"
        output.mkdir(parents=True, exist_ok=True)
        (output / f"{args.mode}-process.log").write_bytes(result.stdout + result.stderr)
        if result.returncode:
            (output / f"{args.mode}-receipt.json").write_text(json.dumps({"status":"current-fail","platform":sys.platform,"exit":result.returncode}),encoding="utf-8")
            print((result.stdout + result.stderr).decode(errors="replace"))
            return 1
        value = json.loads((directory / "result.json").read_text(encoding="utf-8"))
        (output / f"{args.mode}-receipt.json").write_text(json.dumps({"status":"current-pass","platform":sys.platform,"observed":value}, indent=2), encoding="utf-8")
        capture = os.environ.get("SEMPL_CAPTURE_DIR") or os.environ.get("SEMPL_CHECK_CAPTURE_DIR")
        if capture:
            with (Path(capture) / f"cli-signals-{args.mode}.json").open("x", encoding="utf-8") as stream:
                json.dump(value, stream, indent=2)
        if not os.environ.get("SEMPL_CAPTURE_DIR"):
            expected = json.loads((ROOT / "tests/frozen/0.4.0/observed.json").read_text(encoding="utf-8"))["cases"][f"cli-signals-{args.mode}"]
            assert_observation(f"cli-signals-{args.mode}", value, expected)
        print("CLI OS signals: current-pass")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
