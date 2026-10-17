#!/usr/bin/env python3
"""Build an offline linked assembly viewer from RSEMU_DIRECT_DUMP output.

Requires objdump and riscv64-linux-gnu-objdump. Example:
  RSEMU_DIRECT_DUMP=/tmp/blocks target/release/rsemu ... -jit-backend direct
  python3 scripts/jit-view.py /tmp/blocks/PC.json --output viewer.html
"""
import argparse
import json
from pathlib import Path
import re
import subprocess
import tempfile


def disassemble(path, machine, tool, base=0):
    text = subprocess.check_output(
        [tool, '-D', '-b', 'binary', '-m', machine, '--no-show-raw-insn',
         '--insn-width=16', f'--adjust-vma={base}', str(path)]
        + (['-M', 'intel'] if machine == 'i386:x86-64' else []), text=True)
    return [(int(m[1], 16), m[2].strip()) for line in text.splitlines()
            if (m := re.match(r'^\s*([0-9a-f]+):\s+(.+)$', line))]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('dump', type=Path)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--profile-note', default='', help='Measured profile context (not inferred from code)')
    args = p.parse_args()
    data = json.loads(args.dump.read_text())
    data['profile_note'] = args.profile_note
    with tempfile.TemporaryDirectory() as temporary:
        binary = Path(temporary) / 'code.bin'
        binary.write_bytes(bytes(data['code']))
        data['native'] = [{'offset': offset, 'text': text} for offset, text in
                          disassemble(binary, 'i386:x86-64', 'objdump')]
        # Decode each guest instruction separately: traces may contain gaps.
        for guest in data['guest']:
            binary.write_bytes(guest['raw'].to_bytes(4, 'little')[:guest['size']])
            decoded = disassemble(binary, 'riscv:rv64', 'riscv64-linux-gnu-objdump',
                                  int(guest['pc'], 16))
            guest['text'] = decoded[0][1]
    # Every instruction boundary must be owned by exactly one region.
    boundaries = {row['offset'] for row in data['native']} | {len(data['code'])}
    previous = data['entry_end']
    for span in data['spans']:
        assert span['start'] == previous
        assert span['start'] in boundaries and span['end'] in boundaries
        previous = span['end']
    assert previous == data['body_end']
    for hoist in data.get('hoists', []):
        assert hoist['start'] in boundaries and hoist['end'] in boundaries
    template = Path(__file__).with_suffix('.html').read_text()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(template.replace('/*DATA*/null', json.dumps(data).replace('<', '\\u003c')))
    print(f"{len(data['guest'])} guest instructions → {len(data['native'])} host instructions; {len(data['code'])} bytes")


if __name__ == '__main__':
    main()
