#!/usr/bin/env python3
"""ZIP half of refresh-environment-sdk.mjs; preserve the pinned SDK's other files."""
import hashlib
import json
from pathlib import Path
import sys
import zipfile

BASE = {
    'node_modules/@dcl/js-runtime/index.d.ts': '648cafdf95b3ab509fcea4f8d8d1d28637c814203e00d5ecb74b622ef81711d7',
    'node_modules/@dcl/ecs/dist-cjs/components/generated/types.gen.js': '1fe5e7cfcaf3fe77d3304be0f454ff10e04ee43fd603f729fe7ecae5822e714a',
    'node_modules/@dcl/ecs/dist-cjs/components/generated/component-names.gen.js': '7a2a0bbb343c6a883897213ae7b795b033cf1b8866fac0fe27cea88cb9a1685a',
    'node_modules/@dcl/sdk/prebuilt/core.js': '9322b30b0877230b3e6e95a3cc903579930bba2ada4fe914d9f521aa0c085c9a',
    'node_modules/@dcl/ecs/dist-cjs/components/generated/index.gen.js': 'f09cd9e1922b887ad0891fc81a0760929e8985ae38671a408964def1af91d11d',
    'node_modules/@dcl/ecs/dist-cjs/components/generated/global.gen.js': '6744f0f6183d8f4a5c744de4c4e61bd9dcb4473167208b64ab42a094635dc596',
}
archive, payload_file = map(Path, sys.argv[1:])
payload = json.loads(payload_file.read_text())
marker = payload['marker'].encode()
sha = lambda value: hashlib.sha256(value).hexdigest()
with zipfile.ZipFile(archive) as source:
    infos = {entry.filename: entry for entry in source.infolist()}
    assert len(infos) == len(source.infolist()), 'duplicate ZIP names'
    entries = {name: source.read(name) for name in infos}
original = entries.copy()
declarations_name = 'node_modules/@dcl/js-runtime/index.d.ts'
declarations_anchor = b'      "core::SkyboxTime": LwwComponentGetter<LastWriteWinElementSetComponentDefinition<PBSkyboxTime>>;'
declarations_fields = ''.join(f'      "{name}": typeof import("@dcl/ecs/dist/components/generated/index.gen").{name.split("::")[1]};\n' for name in payload['selectedMappings']).encode()
entries[declarations_name] = entries[declarations_name].replace(declarations_fields + declarations_anchor, declarations_anchor, 1)

global_name = 'node_modules/@dcl/ecs/dist-cjs/components/generated/global.gen.js'
global_prelude = 'exports.Skybox = exports.Sun = exports.EnvironmentFog = void 0;\n'.encode()
entries[global_name] = entries[global_name].replace(b'"use strict";\n' + global_prelude, b'"use strict";\n', 1)
core_name = 'node_modules/@dcl/sdk/prebuilt/core.js'
anchor = b'"core::SkyboxTime":1210,'
mapping_fields = json.dumps(payload['selectedMappings'], separators=(',', ':'))[1:-1].encode() + b','
entries[core_name] = entries[core_name].replace(mapping_fields + anchor, anchor, 1)
for name, digest in BASE.items():
    baseline = entries[name].split(marker)[0]
    assert sha(baseline) == digest, f'pinned SDK input changed: {name}; review before refreshing'
    entries[name] = baseline
for name, content in payload['files'].items():
    # Existing common codecs belong to the pinned SDK; these components use their public codec API.
    if '/pb/decentraland/common/' in name and name in entries and not name.endswith('/gradient.gen.js'):
        continue
    entries[name] = content.encode()
assert entries[core_name].count(anchor) == 1, 'pinned numeric component map must have exactly one anchor'
entries[core_name] = entries[core_name].replace(anchor, mapping_fields + anchor, 1)
entries[global_name] = entries[global_name].replace(b'"use strict";\n', b'"use strict";\n' + global_prelude, 1)
assert entries[declarations_name].count(declarations_anchor) == 1, 'pinned declaration component map must have exactly one anchor'
entries[declarations_name] = entries[declarations_name].replace(declarations_anchor, declarations_fields + declarations_anchor, 1)
additions = {
    'node_modules/@dcl/js-runtime/index.d.ts': payload['declarationsAppend'],
    'node_modules/@dcl/ecs/dist-cjs/components/generated/types.gen.js': payload['typesAppend'],
    core_name: payload['extension'],
    'node_modules/@dcl/ecs/dist-cjs/components/generated/index.gen.js': payload['indexAppend'],
    'node_modules/@dcl/ecs/dist-cjs/components/generated/global.gen.js': payload['globalAppend'],
    'node_modules/@dcl/ecs/dist-cjs/components/generated/component-names.gen.js':
        'Object.assign(exports.coreComponentMappings, ' + json.dumps(payload['selectedMappings']) + ');\n',
}
for name, content in additions.items():
    entries[name] += marker + content.encode()
manifest_name = 'node_modules/@dcl/sdk/prebuilt/environment-bindings.json'
manifest = {'baseSdk': '7.29.1-34986384248.commit-bb45080', 'componentIds': [1223, 1224, 1225],
            'baseFiles': BASE, 'generatedSources': payload['inputs'],
            'changedFiles': {name: sha(data) for name, data in entries.items()
                             if name in BASE or name in payload['files']}}
entries[manifest_name] = (json.dumps(manifest, indent=2, sort_keys=True) + '\n').encode()
output = archive.with_suffix('.zip.tmp')
with zipfile.ZipFile(output, 'w', compression=zipfile.ZIP_DEFLATED, compresslevel=9) as target:
    for name in sorted(entries):
        info = infos.get(name)
        if info is None:
            info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o100644 << 16
        target.writestr(info, entries[name])
with zipfile.ZipFile(output) as check:
    for name, before in original.items():
        if name not in BASE and name not in payload['files'] and name != manifest_name:
            assert check.read(name) == before, f'unrelated SDK file changed: {name}'
output.replace(archive)
print(json.dumps({'archiveSha256': sha(archive.read_bytes()), 'bytes': archive.stat().st_size,
                  'changed': [n for n in entries if original.get(n) != entries[n]]}, indent=2))
