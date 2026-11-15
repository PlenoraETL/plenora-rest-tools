"""Real POSIX permission and file-size failures; run unprivileged in the Linux VM."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading

ROOT = Path(__file__).resolve().parents[1]


def main():
    output = ROOT / "target/frozen/linux-io"
    output.mkdir(parents=True, exist_ok=True)
    if sys.platform != "linux" or os.geteuid() == 0:
        (output / "receipt.json").write_text(json.dumps({"status":"skip", "reason":"requires unprivileged Linux"}))
        return 77
    import resource
    receipts = []
    with tempfile.TemporaryDirectory(dir=ROOT / "target",prefix="linux-io-") as temporary:
        directory = Path(temporary)
        sink = directory / "PRIVATE_DIRECTORY_SENTINEL"
        sink.mkdir()
        config = directory / "config.json"
        config.write_text(json.dumps({"allow_private_networks":True,"allow_file_transfers":True,"file_root":str(sink)}))
        for mode, expected in [("open","FILE_IO"),("write-flush","DOWNLOAD_WRITE_FAILED")]:
            requests = []
            class Handler(BaseHTTPRequestHandler):
                def do_GET(self):
                    requests.append(self.path)
                    body = b"PRIVATE_ARTIFACT_SENTINEL" * 4096
                    self.send_response(200)
                    self.send_header("Content-Length",str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                def log_message(self, *args):
                    pass
            server = ThreadingHTTPServer(("127.0.0.1",0),Handler)
            thread = threading.Thread(target=server.serve_forever,daemon=True)
            thread.start()
            def limit():
                signal.signal(signal.SIGXFSZ, signal.SIG_IGN)
                resource.setrlimit(resource.RLIMIT_FSIZE, (512,512))
            try:
                if mode == "open":
                    sink.chmod(0o500)
                request = {"schema_version":1,"operation":"download","connection":{"url":f"http://127.0.0.1:{server.server_port}/"},"input":{"file":{"path":"PRIVATE_FILE_SENTINEL"}}}
                result = subprocess.run([str(ROOT / "target/debug/plenora-rest"),"download","--input","-","--config",str(config),"--format","json"],
                    input=json.dumps(request).encode(),capture_output=True,timeout=15,
                    preexec_fn=limit if mode != "open" else None)
                value = json.loads(result.stdout)
                assert result.returncode != 0 and value["error"]["code"] == expected, (result.returncode,value)
                assert b"PRIVATE_" not in result.stdout + result.stderr
                assert result.stderr == b"", result.stderr
                assert len(requests) == (0 if mode == "open" else 1), requests
                assert list(sink.iterdir()) == [], "staging or publication survived failure"
                receipts.append({"id":mode,"status":"current-pass","exit":result.returncode,"observed":value})
            finally:
                sink.chmod(0o700)
                server.shutdown()
                server.server_close()
                thread.join()
    (output / "receipt.json").write_text(json.dumps({"status":"current-pass","tests":receipts},indent=2)+"\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
