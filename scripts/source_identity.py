"""SHA-256 identities of Git-canonical sources; artifacts keep their raw bytes."""
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def byte_digest(path):
    """Exact bytes: wheels, native modules and mutation restoration evidence."""
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def source_digests(paths, root=ROOT):
    """Hash clean Git blobs using only the source tree's .gitattributes.

    A disposable repository avoids touching the real index/object store and
    also works in Docker's source copy, which deliberately excludes .git.
    Git itself handles text=auto, -text, nested attributes and binary detection;
    no newline heuristic or JSON reserialization stands in for those rules.
    User/global Git settings cannot change the source identity.
    """
    root = Path(root).resolve()
    paths = [Path(path).resolve() for path in paths]
    relative = [path.relative_to(root).as_posix() for path in paths]
    if not (root / ".gitattributes").is_file():
        raise ValueError("source identities require the repository attributes")
    temporary = root / "target/tmp"
    temporary.mkdir(parents=True, exist_ok=True)
    # Drop inherited repository/config overrides: this repository is isolated.
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull, GIT_ATTR_NOSYSTEM="1")
    with tempfile.TemporaryDirectory(dir=temporary, prefix="source-identity-") as directory:
        def git(*arguments, data=None):
            return subprocess.run(
                ["git", "-c", "core.autocrlf=false", "-c", "core.safecrlf=false",
                 "-c", "core.attributesFile=" + os.devnull,
                 "--git-dir=" + directory, "--work-tree=" + str(root), *arguments],
                cwd=root, env=env, input=data, capture_output=True, check=True,
            ).stdout
        subprocess.run(["git", "init", "--bare", "--quiet", directory],
                       cwd=root, env=env, capture_output=True, check=True)
        result = {}
        for path, name in zip(paths, relative):
            oid = git("hash-object", "-w", "--stdin", "--path=" + name,
                      data=path.read_bytes()).decode("ascii").strip()
            result[path] = hashlib.sha256(git("cat-file", "blob", oid)).hexdigest()
        return result


def source_digest(path, root=ROOT):
    path = Path(path).resolve()
    return source_digests([path], root)[path]
