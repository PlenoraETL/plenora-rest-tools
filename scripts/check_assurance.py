"""Offline source-derived assurance inventories; never invokes Cargo.

Rust extraction is deliberately limited to the declarations used here. A changed
shape fails closed and requires adapting the extractor, not a guessed inventory.
Historical campaign reports are observations of their recorded commit, not current
qualification. They are inventoried but never presented as current passing tests.
"""
from __future__ import annotations

import importlib.util
import ast
import json
from pathlib import Path
import re
import sys
import tomllib

import yaml

ROOT = Path(__file__).resolve().parents[1]


class WorkflowLoader(yaml.BaseLoader):
    """Reject duplicate keys instead of silently inspecting only the last job."""


def unique_pairs(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate inventory key")
        result[key] = value
    return result


def workflow_mapping(loader, node):
    return unique_pairs((loader.construct_object(k), loader.construct_object(v)) for k, v in node.value)


WorkflowLoader.add_constructor(yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG, workflow_mapping)


def document(text):
    return json.loads(text, object_pairs_hook=unique_pairs)


def read(root, path):
    return (root / path).read_text(encoding="utf-8")


def same(actual, expected, label):
    if sorted(actual) != sorted(expected):
        raise ValueError(f"{label}: inventory differs from sources")


def capture(pattern, source):
    matches = re.findall(pattern, source, re.S)
    if len(matches) != 1:
        raise ValueError("source declaration absent or ambiguous")
    return matches[0]


def workflows(root):
    # BaseLoader preserves 'on' as a string (YAML 1.1 safe_load treats it as bool).
    return {p.name: yaml.load(p.read_text(encoding="utf-8"), Loader=WorkflowLoader)
            for p in sorted((root / ".github/workflows").glob("*.y*ml"))}


def check_fuzz(root):
    manifest = tomllib.loads(read(root, "fuzz/Cargo.toml"))
    targets = [item["name"] for item in manifest["bin"]]
    same(targets, [p.stem for p in (root / "fuzz/fuzz_targets").glob("*.rs")], "fuzz binaries")
    for item in manifest["bin"]:
        if not (root / "fuzz" / item["path"]).is_file():
            raise ValueError("fuzz target path missing")
    same(workflows(root)["fuzz.yml"]["jobs"]["campaign"]["strategy"]["matrix"]["target"],
         targets, "fuzz matrix")
    same(re.findall(r"^\| `([a-z_]+)` \|", read(root, "fuzz/README.md"), re.M), targets, "fuzz documentation")
    spec = importlib.util.spec_from_file_location("corpus_generator", root / "fuzz/genera_corpus.py")
    generator = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(generator)
    expected = generator.seed_inventory()
    same(expected, targets, "corpus generators")
    files = {p.relative_to(root / "fuzz/corpus").as_posix(): p.read_bytes()
             for p in (root / "fuzz/corpus").rglob("*") if p.is_file()}
    generated = {f"{target}/{name}": data for target, seeds in expected.items()
                 for name, data in seeds.items()}
    if files != generated or any(not seeds for seeds in expected.values()):
        raise ValueError("seed corpus differs from generator (including extra files)")


def check_campaign(root):
    source = read(root, "crates/rest-campaign/src/scenarios.rs")
    body = capture(r"\nscenarios!\s*\{([^}]+)\}", source)
    names = re.findall(r'\w+\s*=>\s*"([a-z_0-9]+)"', body)
    if not names:
        raise ValueError("empty scenario declaration")
    config = read(root, "crates/rest-campaign/src/config.rs")
    phases = re.findall(r'"(\w+)"\s*=>\s*Some\(Self::', config)
    profiles = document(read(root, "campaign/profiles.json"))
    same(set(profiles) - {"schema", "quick"}, phases, "campaign profiles")
    for phase in phases:
        same(profiles[phase]["mix"], names, f"campaign {phase} mix")
    limits = document(read(root, "campaign/limits.json"))
    if not set(limits["latency_p99_ms"]) <= set(names):
        raise ValueError("latency budget names an absent scenario")
    if not set(limits["trend_required_phases"]) <= set(phases):
        raise ValueError("trend budget names an absent phase")
    same(set(limits) - {"schema", "approval", "motivations"}, limits["motivations"], "limit motivations")
    options = workflows(root)["campaign.yml"]["on"]["workflow_dispatch"]["inputs"]["phase"]["options"]
    same(options, phases, "campaign dispatch")
    steps = workflows(root)["campaign.yml"]["jobs"]["campaign"]["steps"]
    for event in ("schedule", "pull_request"):
        blocks = [step["run"] for step in steps if step.get("if") == f"github.event_name == '{event}'"]
        if len(blocks) != 1:
            raise ValueError("campaign collection block absent or ambiguous")
        lines = blocks[0].strip().splitlines()
        if lines[0] != "status=0" or lines[-1] != 'exit "$status"':
            raise ValueError("campaign must report the aggregate failure status")
        for line in lines[1:-1]:
            if not re.fullmatch(r"\./scripts/campaign\.sh \w+ --[\w-]+(?: \d+)? \|\| status=1", line):
                raise ValueError("campaign must continue after each phase failure")
        scheduled = [line.split()[1] for line in lines[1:-1]]
        # Soak exceeds the short campaign's time budget; it remains dispatchable.
        same(scheduled, set(phases) - {"soak"}, "short campaign phases")


def check_runtime(root):
    source = read(root, "crates/rest-engine-core/tests/runtime_vectors.rs")
    pins = document(read(root, "contracts/upstream/source.json"))["files"]
    probes = [p for p in pins if p.startswith("runtime-probes-v1/")]
    same(probes, [p.relative_to(root / "contracts/upstream").as_posix()
                  for p in (root / "contracts/upstream/runtime-probes-v1").glob("*.json")], "runtime probes")
    total = int(capture(r'\.count\(\),\s*(\d+),\s*"every rejection probe', source))
    exercised = int(capture(r'assert_eq!\(exercised, (\d+),', source))
    skipped = re.findall(r'if name\.ends_with\("([^"]+)"\)\s*\{\s*continue;', source)
    if total != len(probes) or exercised != sum(not any(p.endswith(s) for s in skipped) for p in probes):
        raise ValueError("Rust probe counts differ from adopted fixtures")
    for skip in skipped:
        if sum(p.endswith(skip) for p in probes) != 1:
            raise ValueError("probe exclusion is missing or ambiguous")
    own = sum(document(read(root, "contracts/upstream/" + p))["base"].startswith("rest-") for p in probes)
    expected_own = int(capture(r'assert_eq!\(\s*own_base, (\d+),', source))
    if own != expected_own:
        raise ValueError("REST base probe count differs")
    vectors = re.findall(r'const \w+: &str = "(runtime-v1/[^"]+)";', source)
    same(vectors, [p for p in pins if p.startswith("runtime-v1/")], "exercised REST vectors")


def check_packages(root):
    registry = document(read(root, "scripts/package_files.json"))
    workspace = tomllib.loads(read(root, "Cargo.toml"))["workspace"]["members"]
    names = []
    for member in workspace:
        name = tomllib.loads(read(root, member + "/Cargo.toml"))["package"]["name"]
        names.append(name)
        same([p for p in registry[name]["files"] if p.startswith("src/")],
             [p.relative_to(root / member).as_posix() for p in (root / member / "src").rglob("*") if p.is_file()],
             f"package sources {name}")
    same(registry, names, "workspace packages")


def check_evidence(root):
    requirements = document(read(root, "contracts/adoption-requirements.json"))
    adopted = document(read(root, "contracts/upstream/source.json"))
    for entry in requirements["requirements"]:
        if "upstream_context" in entry:
            context = entry["upstream_context"]
            if any(context[key] != adopted[key] for key in ("repository", "revision")):
                raise ValueError("external context differs from adopted revision")
            if not context["path"] or ":" in context["path"] or ".." in context["path"].split("/"):
                raise ValueError("external context must name an upstream repository path")
        for reference in entry["evidence"].split("; "):
            path, separator, symbol = reference.partition("::")
            if (not (root / path).is_file() or Path(path).is_absolute()
                    or not (root / path).resolve().is_relative_to(root.resolve())):
                raise ValueError(f"{entry['rule']}: evidence must name a repository file")
            if separator and not re.search(r"\b(?:async\s+)?fn\s+" + re.escape(symbol) + r"\s*\(", read(root, path)):
                raise ValueError(f"{entry['rule']}: evidence test is absent")


def check_workflows(root):
    flows = workflows(root)
    job = flows["verify.yml"]["jobs"]["assurance"]
    if flows["verify.yml"]["on"]["pull_request"] or "if" in job or "needs" in job:
        raise ValueError("assurance must run unconditionally on every PR")
    if any(item.get("continue-on-error", "false") != "false" for item in [job, *job["steps"]]):
        raise ValueError("assurance failures must fail the PR job")
    runs = [step.get("run", "") for step in job["steps"] if "if" not in step]
    if "python scripts/run_assurance.py" not in runs:
        raise ValueError("PR assurance runner is not connected")
    project = tomllib.loads(read(root, "pyproject.toml"))["project"]
    bounds = re.fullmatch(r">=3\.(\d+),<3\.(\d+)", project["requires-python"])
    if bounds is None:
        raise ValueError("unsupported Python support range syntax")
    supported = [f"3.{minor}" for minor in range(int(bounds[1]), int(bounds[2]))]
    if not supported:
        raise ValueError("empty Python support range")
    same(re.findall(r'Programming Language :: Python :: (3\.\d+)"', read(root, "pyproject.toml")),
         supported, "Python classifiers")
    for name, flow in flows.items():
        for job in flow["jobs"].values():
            for step in job["steps"]:
                if "unittest discover -s scripts" in step.get("run", "") and "test_check_" in step["run"]:
                    raise ValueError("workflow self-test discovery omits contract tests")
            strategy = job.get("strategy", {})
            if strategy and strategy.get("fail-fast") != "false":
                raise ValueError(f"{name}: matrix must collect all failures")
            matrix = strategy.get("matrix", {})
            if "python" in matrix:
                same(matrix["python"], supported, f"{name} Python support")
    tags = capture(r"for tag in ([^;]+); do", read(root, "Dockerfile.release"))
    same(re.findall(r"cp3(\d+)-cp3\d+", tags), [v.split(".")[1] for v in supported], "Linux ABI3 matrix")


def check_tool_pins(root):
    pins = dict(re.findall(r"^(\w+)==([\d.]+)$", read(root, "scripts/assurance-requirements.txt"), re.M))
    for path in [root / "Dockerfile.verify", *sorted((root / ".github/workflows").glob("*.yml"))]:
        for name, version in re.findall(r"(jsonschema|PyYAML)==([\d.]+)", path.read_text(encoding="utf-8")):
            if pins[name] != version:
                raise ValueError("assurance tool pin differs from canonical requirements")


def check_script_discovery(root):
    import run_assurance
    commands = run_assurance.commands()
    if not any(command[1:] == ["-m", "unittest", "discover", "-s", "scripts", "-p", "test_*.py", "-v"]
               for command in commands):
        raise ValueError("runner must discover every script self-test")
    for path in sorted((root / "scripts").glob("*.py")):
        tree = ast.parse(path.read_text(encoding="utf-8"))
        tests = [node for node in ast.walk(tree) if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))
                 and node.name.startswith("test_")]
        if tests and not path.name.startswith("test_"):
            raise ValueError("self-test module is outside unittest discovery")


def check_coverage_sources(root):
    budget = document(read(root, "scripts/coverage_budget.json"))["surfaces"]
    members = tomllib.loads(read(root, "Cargo.toml"))["workspace"]["members"]
    # Workspace publish policy is not an assurance classification: core, CLI and
    # binding are shipped; the operational harness is explicitly excluded.
    shipped = [member + "/src/" for member in members if member != "crates/rest-campaign"]
    actual = [source for entry in budget.values() if entry["report_format"] == "llvm" for source in entry["sources"]]
    same(actual, shipped, "coverage Rust surfaces")
    sdk = tomllib.loads(read(root, "pyproject.toml"))["tool"]["maturin"]
    package = sdk["module-name"].split(".")[0]
    same(budget["python_sdk"]["sources"], [package + "/"], "coverage SDK surface")
    if not (root / sdk["python-source"] / package).is_dir():
        raise ValueError("coverage SDK source is absent")


CHECKS = (check_fuzz, check_campaign, check_runtime, check_packages, check_evidence,
          check_workflows, check_tool_pins, check_script_discovery, check_coverage_sources)


def scan(root=ROOT):
    errors = []
    for check in CHECKS:
        try:
            check(root)
        except (ValueError, KeyError, TypeError, OSError, yaml.YAMLError, SyntaxError) as error:
            errors.append(f"{check.__name__}: {error}")
    return errors


def inventory(root=ROOT):
    patterns = ("scripts/*.py", "scripts/*.ps1", "scripts/*.sh", ".github/workflows/*.y*ml",
                "fuzz/fuzz_targets/*.rs", "fuzz/corpus/**/*", "contracts/upstream/**/*.json",
                "crates/**/*.rs", "python/tests/*.py", "campaign/reports/*",
                "contracts/*.json", "contracts/schemas/*.json", "scripts/*.json",
                "crates/**/tests/fixtures/**/*", "campaign/*.json", "fuzz/comune/*.rs",
                "scripts/*requirements.txt")
    return sorted({p.relative_to(root).as_posix() for pattern in patterns for p in root.glob(pattern) if p.is_file()})


if __name__ == "__main__":
    if sys.argv[1:] == ["--inventory"]:
        print(json.dumps(inventory(), indent=2))
    else:
        errors = scan()
        print("\n".join(errors) if errors else "assurance inventories: OK")
        raise SystemExit(bool(errors))
