#!/usr/bin/env python3
# Copyright 2026 Haixing Hu. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Controlled verify boundary: real CMake build, protocol fixture output."""
import hashlib
import fcntl
import json
import pathlib
import re
import subprocess
import sys

args = sys.argv[1:]
def flag(name):
    return args[args.index(name) + 1]
project = pathlib.Path(flag('--project')).resolve()
run_context = json.loads(pathlib.Path(flag('--run-context')).read_text())
source_excluded_paths = set(run_context.get('source_excluded_paths', []))
output = pathlib.Path(flag('--output'))
build = project / 'build/infra/analysis'
build.mkdir(parents=True, exist_ok=True)
lock = (build / '.cpp-infra.lock').open('a')
fcntl.flock(lock, fcntl.LOCK_EX)
def digest(config):
    files = []
    for path in project.rglob('*'):
        rel = path.relative_to(project)
        if path.is_symlink() or not path.is_file() or rel.parts[0] in ('.git', 'build', 'target'):
            continue
        if config:
            if rel.parts[0] == '.infra' and len(rel.parts) > 1 and rel.parts[1] in ('.tmp', 'transactions', '.transactions'):
                continue
            if rel.parts[0] != '.infra' and str(rel) not in ('CMakeLists.txt', 'CMakePresets.json', 'conanfile.py', 'conanfile.txt') and rel.parts[0] != 'conan':
                continue
        elif rel.parts[0] == '.infra':
            continue
        elif str(rel) in source_excluded_paths:
            continue
        files.append((str(rel), path.read_bytes()))
    h = hashlib.sha256()
    for name, content in sorted(files):
        raw = name.encode()
        h.update(len(raw).to_bytes(8, 'little')); h.update(raw)
        h.update(len(content).to_bytes(8, 'little')); h.update(content)
    return h.hexdigest()
def sha(path):
    return hashlib.sha256(pathlib.Path(path).read_bytes()).hexdigest()
if run_context.get('build_context'):
    context = json.loads(pathlib.Path(run_context['build_context']).read_text())
    state = json.loads((build / '.cpp-infra-build-state.json').read_text())
    valid = (context.get('source_digest') == digest(False) and context.get('config_digest') == digest(True)
             and context.get('source_excluded_paths', []) == sorted(source_excluded_paths)
             and context.get('configuration_inputs') and context.get('cmake_cache_digest') == sha(build / 'CMakeCache.txt')
             and state.get('fingerprint') == context.get('fingerprint'))
    if not valid or any(sha(item['path']) != item['sha256'] for item in context['configuration_inputs']):
        raise SystemExit(2)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(context))
    pathlib.Path(flag('--report')).write_text(json.dumps(dict(schema=1,status='passed',data=dict(reused_build=True))))
    raise SystemExit(0)
subprocess.run(['cmake', '-S', str(project), '-B', str(build), '-G', 'Ninja', '-DCMAKE_CXX_COMPILER=/usr/bin/clang++-18', '-DCMAKE_EXPORT_COMPILE_COMMANDS=ON'], check=True)
subprocess.run(['cmake', '--build', str(build)], check=True)
db = json.loads((build / 'compile_commands.json').read_text())
targets = {}
for entry in db:
    name = re.search(r'CMakeFiles/(.+?)\.dir/', entry.get('command', '')).group(1)
    target = targets.setdefault(name, dict(name=name, type='STATIC_LIBRARY', sources=[], artifacts=[str(build / ('lib' + name + '.a'))]))
    target['sources'].append(entry['file'])
context = dict(schema=1, project=str(project), profile=flag('--profile'), binary_dir=str(build), configure_preset='analysis', build_preset='analysis', test_preset=None, configuration=None, source_digest=digest(False), config_digest=digest(True), source_excluded_paths=sorted(source_excluded_paths), fingerprint='fixture-build-fingerprint', configuration_inputs=[dict(path=str((project / 'CMakeLists.txt').resolve()), sha256=sha(project / 'CMakeLists.txt'), kind='project')], cmake_cache_digest=sha(build / 'CMakeCache.txt'), compiler=dict(id='Clang', path='/usr/bin/clang++-18', version='18.1.8'), generator='Ninja', compile_commands=str(build / 'compile_commands.json'), targets=list(targets.values()))
(build / '.cpp-infra-project.json').write_text(json.dumps(dict(schema=1, project=str(project))))
(build / '.cpp-infra-build-state.json').write_text(json.dumps({key: context[key] for key in ('schema', 'project', 'profile', 'source_digest', 'config_digest', 'source_excluded_paths', 'fingerprint', 'configuration_inputs', 'cmake_cache_digest')}))
output.parent.mkdir(parents=True, exist_ok=True)
output.write_text(json.dumps(context))
pathlib.Path(flag('--report')).write_text(json.dumps(dict(schema=1, status='passed')))
