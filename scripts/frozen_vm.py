"""Collect all targeted VM outcomes, including every control case after a failure."""
from datetime import datetime, timezone
import json
from pathlib import Path
import subprocess
import sys

from frozen_rust import ROOT, environment, executable, rust_passed
from mutate_frozen_rust import MUTATIONS


class Collection:
    def __init__(self, directory, env):
        self.directory, self.env, self.results = directory, env, []
        directory.mkdir(parents=True, exist_ok=True)

    def save(self):
        passed = bool(self.results) and all(r["status"] == "current-pass" for r in self.results)
        (self.directory / "receipt.json").write_text(json.dumps({"passed":passed,"tests":self.results},indent=2)+"\n",encoding="utf-8")
        return passed

    def blocked(self, identity, reason):
        self.results.append({"id":identity,"status":"blocked","reason":reason})
        self.save()

    def run(self, identity, command, *, env=None, rust=False):
        print(f"RUN {identity}", flush=True)
        try:
            result = subprocess.run(command,cwd=ROOT,env=env or self.env,capture_output=True,text=True,encoding="utf-8",errors="replace")
            log = result.stdout + result.stderr
            passed = rust_passed(result.returncode,log) if rust else result.returncode == 0
            status = "current-pass" if passed else ("skip" if result.returncode == 77 else "current-fail")
            row = {"id":identity,"status":status,"exit":result.returncode,"command":list(map(str,command))}
        except OSError as error:
            log, row = str(error), {"id":identity,"status":"blocked","reason":str(error)}
        (self.directory / f"{identity}.log").write_text(log,encoding="utf-8")
        self.results.append(row)
        self.save()
        print(log, end="",flush=True)
        print(f"RESULT {identity}: {row['status']}",flush=True)
        return row["status"] == "current-pass"


def main():
    env = environment()
    for name in ("SEMPL_CAPTURE_DIR","SEMPL_CHECK_CAPTURE_DIR","SEMPL_CONTROL_CASE"):
        env.pop(name,None)
    directory = ROOT / "target/frozen/linux" / datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
    collection = Collection(directory,env)
    try:
        binary = executable("plenora-rest-core","--lib",None,env)
        listed = subprocess.check_output([binary,"injected_transport::","--list"],env=env,text=True)
        ids = [line.removesuffix(": test") for line in listed.splitlines() if line.endswith(": test")]
        if not ids:
            raise ValueError("empty control test discovery")
        for identity in ids:
            if identity.endswith("frozen_controls_visit_every_declared_branch"):
                corpus = json.loads((ROOT / "tests/frozen/0.4.0/io-branches.json").read_text(encoding="utf-8"))
                for branch in corpus["branches"]:
                    for action in ("cancel","deadline"):
                        case = branch["point"] + "-" + action
                        collection.run("control-"+case,[binary,identity,"--exact","--nocapture"],env={**env,"SEMPL_CONTROL_CASE":case},rust=True)
            else:
                collection.run(identity.split("::")[-1],[binary,identity,"--exact","--nocapture"],rust=True)
    except (OSError,RuntimeError,ValueError,subprocess.SubprocessError) as error:
        collection.blocked("injected-build-discovery",str(error))
    collection.run("remote",["cargo","test","-p","plenora-rest-core","--test","black_box","frozen_remote_body_and_sensitive_header_matrix","--locked","--offline","--","--nocapture"],rust=True)
    built = collection.run("cli-build",["cargo","build","-p","plenora-rest-cli","--bin","plenora-rest","--locked","--offline"])
    for identity, script in (("signals","frozen_signals.py"),("io","frozen_linux_io.py")):
        if built:
            collection.run(identity,[sys.executable,str(ROOT / "scripts" / script)])
        else:
            collection.blocked(identity,"CLI build failed; do not execute a stale binary")
    for mutation in MUTATIONS:
        if mutation[0].startswith("branches-"):
            collection.run(mutation[0],[sys.executable,str(ROOT / "scripts/mutate_frozen_rust.py"),"--only",mutation[0]])
    collection.run("signal-mutations",[sys.executable,str(ROOT / "scripts/mutate_frozen_signals.py")])
    passed = collection.save()
    (directory.parent / "latest.json").write_text(json.dumps({"receipt":str(directory / "receipt.json"),"passed":passed})+"\n",encoding="utf-8")
    print(f"Complete collection: {directory / 'receipt.json'}; passed={passed}",flush=True)
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
