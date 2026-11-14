#!/usr/bin/env python3
"""Gate offline Plenora, Python >=3.11; analisi del lock completo, non del binario."""
import argparse
import datetime as dt
import hashlib
import json
import pathlib
import re
import sys
import tomllib
from collections import defaultdict

REGISTRY = 'registry+https://github.com/rust-lang/crates.io-index'
SCHEMA = 1

def version_key(value):
    core, _, pre = value.partition('+')[0].partition('-')
    nums = tuple(int(x) for x in core.split('.'))
    return nums + (1 if not pre else 0, pre)

def line(value):
    semver = value.split('+')[0]
    a, b, c = semver.split('-')[0].split('.')
    # Cargo: 0.0.x non e' compatibile con 0.0.y; prerelease separata.
    result = a if a != '0' else f'0.{b}' if b != '0' else f'0.0.{c}'
    return result + ('-' + semver.split('-', 1)[1] if '-' in semver else '')

def identity(p):
    return p['name'], p['version'], p.get('source', 'path')

def label(p):
    return p['name'] + '@' + p['version']

def graph(packages):
    """Risoluzione Cargo.lock delle forme name, name version, name version (source)."""
    parents = defaultdict(list)
    for parent in packages:
        for dep in parent.get('dependencies', []):
            match = re.fullmatch(r'([^ ]+)(?: ([^ ]+))?(?: \((.+)\))?', dep)
            if not match:
                raise ValueError('arco lock non valido')
            name, ver, source = match.groups()
            matches = [p for p in packages if p['name'] == name
                       and (ver is None or p['version'] == ver)
                       and (source is None or p.get('source') == source)]
            if len(matches) != 1:
                raise ValueError('arco lock ambiguo o irrisolto')
            parents[identity(matches[0])].append(parent)
    return parents

def documents(root):
    # Non seguire symlink fuori checkout; target e .git non sono sorgenti.
    for p in sorted(root.rglob('Cargo.*')):
        rel = p.relative_to(root)
        if p.name not in ('Cargo.lock', 'Cargo.toml') or any(x in ('.git', 'target') for x in rel.parts):
            continue
        if p.is_symlink() or not p.resolve().is_relative_to(root.resolve()):
            raise ValueError('file esterno al checkout')
        yield rel.as_posix(), tomllib.loads(p.read_text(encoding='utf8'))

def dependency_tables(doc):
    for section in ('dependencies', 'build-dependencies', 'dev-dependencies'):
        for name, value in doc.get(section, {}).items():
            yield section, 'all', name, value
    for target, tables in doc.get('target', {}).items():
        for section in ('dependencies', 'build-dependencies', 'dev-dependencies'):
            for name, value in tables.get(section, {}).items():
                yield section, target, name, value
    for name, value in doc.get('workspace', {}).get('dependencies', {}).items():
        yield 'workspace.dependencies', 'all', name, value

def fingerprint(f):
    keys = ('repo', 'file', 'rule', 'crate', 'version', 'source', 'parent', 'parent_source', 'detail')
    return hashlib.sha256(json.dumps({k: f.get(k, '') for k in keys}, sort_keys=True).encode()).hexdigest()[:24]

def finding(repo, file, rule, p, **kwargs):
    f = dict(repo=repo, file=file, rule=rule, crate=p['name'],
             version=p.get('version', ''), source=p.get('source', 'path'), **kwargs)
    f['key'] = fingerprint(f)
    return f

def collect(policy, roots):
    findings, inventory = [], {}
    references = {(r['crate'], r['line']): r for r in policy['references']}
    preferred = policy['preferred']
    interfaces = {i['crate']: i for i in policy['interfaces']}
    for sample, root in roots.items():
        repo = sample.split('@', 1)[0]
        start = len(findings)
        docs = dict(documents(root))
        for fork in policy.get('forks', []):
            if fork['repo'] != repo:
                continue
            file = fork['source'].removeprefix('path:') + '/Cargo.toml'
            path = root / file
            if not path.resolve().is_relative_to(root.resolve()):
                raise ValueError('provenienza fork esterna')
            if not path.exists() or hashlib.sha256(path.read_bytes()).hexdigest() != fork['manifest_sha256']:
                findings.append(finding(repo, file, 'fork_provenance',
                    dict(name=fork['crate'], version=fork['version'], source=fork['source']),
                    detail='manifesto fork diverso dallo snapshot approvato'))
        locks = {f: d for f, d in docs.items() if f.endswith('Cargo.lock')}
        if not locks or 'Cargo.toml' not in docs:
            raise ValueError('manifesto o lock radice assente')
        inventory[sample] = sorted(locks)
        for file, doc in locks.items():
            packages = doc.get('package', [])
            parents = graph(packages)
            local_lines = defaultdict(set)
            for p in packages:
                local_lines[p['name']].add(line(p['version']))
            for p in packages:
                name, ver, source = identity(p)
                iface = interfaces.get(name)
                ref = references.get((name, line(ver)))
                def add(rule, **kw):
                    findings.append(finding(repo, file, rule, p, **kw))
                if iface and (ver != iface['version'] or source != iface['source']):
                    add('interface', detail='identita interfaccia diversa dalla baseline')
                if ref:
                    if ver != ref['version']:
                        add('reference', detail='versione diversa dal riferimento della linea')
                    if source != ref['source']:
                        add('source', detail='source diversa dal riferimento')
                    if ver == ref['version'] and source == ref['source'] and ref.get('checksum') and p.get('checksum') != ref['checksum']:
                        add('checksum', detail='checksum lock diverso dalla baseline')
                base_line = preferred.get(name)
                if (base_line and line(ver) != base_line) or (not base_line and len(local_lines[name]) > 1):
                    for parent in parents.get(identity(p), [None]):
                        add('duplicate', parent=label(parent) if parent else '<root>',
                            parent_source=parent.get('source', 'path') if parent else 'path',
                            detail='linea incompatibile nel perimetro di famiglia')
                if name in policy.get('fork_names', []):
                    add('fork', detail='identita fork distinta da upstream')
        # Manifesti propri: i vendor hanno requisiti upstream, non pin Plenora.
        for file, doc in docs.items():
            if not file.endswith('Cargo.toml') or 'vendor' in pathlib.PurePosixPath(file).parts:
                continue
            for section, target, alias, raw in dependency_tables(doc):
                dep = {'version': raw} if isinstance(raw, str) else raw
                if dep.get('workspace'):
                    # L'ereditarieta' punta al workspace antenato piu' vicino.
                    candidates = [(len(pathlib.PurePosixPath(f).parts), d) for f, d in docs.items()
                                  if f.endswith('Cargo.toml') and 'workspace' in d
                                  and pathlib.PurePosixPath(f).parent in pathlib.PurePosixPath(file).parents]
                    nearest = max(candidates, key=lambda pair: pair[0])[1] if candidates else {}
                    if alias not in nearest.get('workspace', {}).get('dependencies', {}):
                        raise ValueError('dipendenza workspace senza dichiarazione')
                    continue
                name = dep.get('package', alias)
                iface = interfaces.get(name)
                requirement = dep.get('version', '')
                if iface:
                    source = 'git' if 'git' in dep else 'path' if 'path' in dep else 'registry:' + dep['registry'] if 'registry' in dep else REGISTRY
                    wrong_path = False
                    if source == 'path':
                        actual_path = (root / pathlib.PurePosixPath(file).parent / dep['path']).resolve()
                        allowed = [(root / f['source'].removeprefix('path:')).resolve()
                                   for f in policy.get('forks', []) if f['repo'] == repo and f['crate'] == name]
                        wrong_path = actual_path not in allowed
                    if requirement != '=' + iface['version'] or source != iface['source'] or wrong_path:
                        findings.append(finding(repo, file, 'manifest_interface',
                            dict(name=name, version=requirement, source=source),
                            detail=section + '|' + target))
                elif requirement.startswith('='):
                    ver = requirement[1:]
                    ref = references.get((name, line(ver)))
                    desired = policy.get('direct_migrations', {}).get(repo, {}).get(name)
                    if (ref and ver != ref['version']) or (desired and ver != desired):
                        findings.append(finding(repo, file, 'manifest_pin',
                            dict(name=name, version=requirement, source=REGISTRY),
                            detail=section + '|' + target))
        for f in findings[start:]:
            f['sample'] = sample
    # Duplicati nuovi che esistono soltanto fra repository: non sfuggono al gate.
    global_packages = defaultdict(list)
    for sample, root in roots.items():
        for file, doc in documents(root):
            if file.endswith('Cargo.lock'):
                for p in doc.get('package', []):
                    if p.get('source'):
                        global_packages[p['name']].append((sample, file, p))
    for name, occurrences in global_packages.items():
        if name not in preferred and len({line(p['version']) for _, _, p in occurrences}) > 1:
            for sample, file, p in occurrences:
                findings.append(finding(sample.split('@', 1)[0], file, 'family_duplicate', p, sample=sample,
                                        detail='nuove linee incompatibili fra lock'))
    return findings, inventory

def evaluate(policy, roots, today=None):
    today = today or dt.date.today()
    raw, inventory = collect(policy, roots)
    exceptions = {x['key']: x for x in policy.get('exceptions', [])}
    pending = {x['key']: x for x in policy.get('alignments', [])}
    used = set()
    errors, accepted = [], []
    for f in raw:
        e = exceptions.get(f['key'])
        # Interfacce e digest non derogabili. Scadenza inclusiva.
        if e and f['rule'] not in ('interface', 'manifest_interface', 'checksum', 'source'):
            used.add((f['sample'], e['key']))
            if dt.date.fromisoformat(e['review_by']) < today:
                errors.append(dict(f, status='SORPRESA', error='eccezione_scaduta'))
            else:
                accepted.append(dict(f, status='ECCEZIONE', exception=e['id']))
        else:
            a = pending.get(f['key'])
            errors.append(dict(f, status='DA_ALLINEARE' if a else 'SORPRESA',
                               release=a['release'] if a else '', error=f['rule']))
    for sample in roots:
        for e in exceptions.values():
            if e['repo'] == sample.split('@', 1)[0] and (sample, e['key']) not in used:
                errors.append(dict(repo=e['repo'], file=e['file'], crate=e['crate'],
                                   version=e['version'], rule='stale_exception',
                                   status='SORPRESA', error='eccezione_non_piu_necessaria', key=e['key'], sample=sample))
    return dict(schema_version=SCHEMA, baseline=policy['baseline'],
                result='FALLITO' if errors else 'SUPERATO', inventory=inventory,
                errors=errors, exceptions=accepted,
                counts=dict(errors=len(errors), alignments=sum(e['status'] == 'DA_ALLINEARE' for e in errors),
                            surprises=sum(e['status'] == 'SORPRESA' for e in errors), exceptions=len(accepted)))

def load_policy(path, digest):
    data = path.read_bytes()
    expected = digest.read_text(encoding='ascii').split()[0]
    if not re.fullmatch('[0-9a-f]{64}', expected) or hashlib.sha256(data).hexdigest() != expected:
        raise ValueError('digest manifesto non corrispondente')
    p = tomllib.loads(data.decode('utf8'))
    if p.get('schema_version') != SCHEMA:
        raise ValueError('schema manifesto non supportato')
    for field in ('interfaces', 'references', 'preferred', 'repositories', 'exceptions', 'alignments'):
        if field not in p:
            raise ValueError('schema manifesto incompleto')
    for field in ('interfaces', 'references'):
        seen = set()
        for r in p[field]:
            key = (r['crate'], r.get('line'))
            if key in seen or not r.get('source') or not r.get('version'):
                raise ValueError('riferimento ambiguo o incompleto')
            seen.add(key)
    if {e['key'] for e in p['exceptions']} & {e['key'] for e in p['alignments']}:
        raise ValueError('registro sovrapposto')
    for field in ('exceptions', 'alignments'):
        keys = set()
        for e in p[field]:
            if e['key'] in keys or e['key'] != fingerprint(e):
                raise ValueError('registro con chiave duplicata o incoerente')
            keys.add(e['key'])
            for required in ('repo', 'crate', 'version', 'source', 'file', 'reason', 'owner'):
                if not e.get(required):
                    raise ValueError('registro incompleto')
            if field == 'exceptions':
                for required in ('chain', 'profiles', 'target', 'removal', 'review_by'):
                    if not e.get(required):
                        raise ValueError('eccezione incompleta')
                dt.date.fromisoformat(e['review_by'])
            elif not e.get('release'):
                raise ValueError('allineamento senza release')
    return p

def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    home = pathlib.Path(__file__).resolve().parent
    parser.add_argument('--manifesto', type=pathlib.Path, default=home / 'plenora-dependencies.toml')
    parser.add_argument('--digest', type=pathlib.Path, default=home / 'plenora-dependencies.sha256')
    parser.add_argument('--repo', type=pathlib.Path)
    parser.add_argument('--nome', help='identificativo repository nel manifesto')
    parser.add_argument('--famiglia', type=pathlib.Path, help='directory contenente i sei checkout/snapshot')
    parser.add_argument('--candidato', action='append', default=[], metavar='NOME=PERCORSO',
                        help='aggiunge un candidato alla famiglia senza sostituire origin/main')
    parser.add_argument('--json', type=pathlib.Path, dest='json_path')
    args = parser.parse_args(argv)
    try:
        policy = load_policy(args.manifesto, args.digest)
        if bool(args.repo) == bool(args.famiglia):
            raise ValueError('specificare --repo oppure --famiglia')
        if args.famiglia:
            roots = {r['name']: args.famiglia / r['name'] for r in policy['repositories']}
            for value in args.candidato:
                name, sep, path = value.partition('=')
                if not sep or not path or name not in roots or name + '@candidato' in roots:
                    raise ValueError('candidato non valido o duplicato')
                roots[name + '@candidato'] = pathlib.Path(path)
        else:
            if args.candidato:
                raise ValueError('candidato richiede famiglia')
            name = args.nome or args.repo.resolve().name
            if name not in {r['name'] for r in policy['repositories']}:
                raise ValueError('repository non dichiarato')
            roots = {name: args.repo}
        report = evaluate(policy, roots)
    except (OSError, ValueError, KeyError, TypeError, IndexError):
        # Non stampare eccezioni Python: possono contenere URL con credenziali o path privati.
        report = dict(result='FALLITO', error='input_schema_o_digest_non_valido',
                      counts=dict(errors=1, alignments=0, surprises=1, exceptions=0))
    # Il report pubblico non contiene source/URL, requisiti o catene libere.
    safe = dict(report)
    for group in ('errors', 'exceptions'):
        safe[group] = [{k: v for k, v in f.items() if k in
                       ('repo', 'sample', 'file', 'rule', 'crate', 'version', 'key', 'status', 'error', 'release', 'exception')}
                      for f in report.get(group, [])]
    if args.json_path:
        args.json_path.write_text(json.dumps(safe, ensure_ascii=False, indent=2) + '\n', encoding='utf8')
    print(report['result'], json.dumps(report['counts']))
    for f in safe['errors']:
        print(f"{f['status']}: {f.get('sample', f['repo'])} / {f['file']} / {f['crate']} {f['version']}: {f['error']}")
    if 'error' in report:
        print(report['error'])
    return 1 if report['result'] == 'FALLITO' else 0

if __name__ == '__main__':
    sys.exit(main())
