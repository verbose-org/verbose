"""Linux x86-64 acceptance: count actual eager/aliased/conditional evaluations.

Run cargo build, then python3 tools/check_bounded_text_storage.py. Requires ptrace
permission. Uses only Python's standard library; artifacts remain in the printed
work directory. Optional --reference-compiler compares reserved frame bytes and
actual bytes copied with a compiler from before destination forwarding. This is
an instruction trace, not a timing benchmark. With --check-reuse, compare with
the compiler before buffer reuse: addresses must be reused and copy counts must
stay unchanged.
"""
import argparse
import collections
import ctypes
import json
import os
from pathlib import Path
import signal
import struct
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--compiler', type=Path, default=ROOT / 'target/debug/verbosec')
parser.add_argument('--reference-compiler', type=Path)
parser.add_argument('--check-reuse', action='store_true',
                    help='trace successive dead buffers; compare with the pre-reuse compiler')
options = parser.parse_args()
WORK = Path(tempfile.mkdtemp(prefix='verbose-text-storage-trace-'))
print(f'Traces: {WORK}', flush=True)
MARKERS = dict(eager=420000000001, once=420000000002,
               yes=420000000003, no=420000000004, probe=420000000005)
REUSE_PREFIXES = {name: f'reuse-{name}:'.ljust(128, name) for name in ['a', 'b', 'c']}
SOURCE = '''@verbose 0.1.0
concept Input
  @intention: "Trace input"
  @source: trace.intent:1
  fields:
    title : text [..8]
    code : number
rule once
  @intention: "Trace an aliased call"
  @source: trace.intent:2
  input:
    i : Input
  output:
    out : text [..28]
  logic:
    let marker = 420000000002
    out = concat(marker, i.title)
  proofs:
    purity:
      reads : [i.title]
      calls : []
    termination:
      bound : 16
rule probe
  @intention: "Trace a short circuit operand"
  @source: trace.intent:3
  input:
    i : Input
  output:
    out : bool
  logic:
    let unused = 420000000005
    out = i.code > 0
  proofs:
    purity:
      reads : [i.code]
      calls : []
    termination:
      bound : 16
rule trace
  @intention: "Trace eager lets and selected branches"
  @source: trace.intent:4
  input:
    req : Input
  output:
    out : text [..76]
  logic:
    let unused = 420000000001
    let result = once(req)
    let saved = result
    let result = "shadow"
    out = if req.code > 0 and probe(req) then concat(saved, saved, 420000000003) else concat(saved, 420000000004)
  proofs:
    purity:
      reads : [req, req.code]
      calls : [once, probe]
    termination:
      bound : 64
rule relay
  @intention: "Forward an inferred-capacity result"
  @source: trace.intent:5
  input:
    other : Input
  output:
    out : text
  logic:
    out = trace(other)
  proofs:
    purity:
      reads : [other]
      calls : [trace]
    termination:
      bound : 8
rule forward
  @intention: "Forward through a wider public result contract"
  @source: trace.intent:6
  input:
    input : Input
  output:
    out : text [..96]
  logic:
    out = relay(input)
  proofs:
    purity:
      reads : [input]
      calls : [relay]
    termination:
      bound : 8
'''
(WORK / 'trace.intent').write_text('Input.\nAliased call.\nShort circuit.\nEager lets and branches.\nInferred relay.\nWider result.\n')

# Linux x86-64 user_regs_struct, as declared by sys/user.h.
class Registers(ctypes.Structure):
    _fields_ = [(n, ctypes.c_ulonglong) for n in '''r15 r14 r13 r12 rbp rbx r11 r10
        r9 r8 rax rcx rdx rsi rdi orig_rax rip cs eflags rsp ss fs_base gs_base
        ds es fs gs'''.split()]

libc = ctypes.CDLL(None, use_errno=True)
libc.ptrace.restype = ctypes.c_long
libc.ptrace.argtypes = [ctypes.c_uint, ctypes.c_uint, ctypes.c_void_p, ctypes.c_void_p]


def ptrace(request, pid=0, data=None):
    if libc.ptrace(request, pid, None, data) == -1:
        err = ctypes.get_errno()
        raise OSError(err, os.strerror(err))


class EvaluationCountMismatch(AssertionError):
    pass


def trace(binary, records, operator, case, reuse_expected=None):
    blob = binary.read_bytes()
    phoff = struct.unpack_from('<Q', blob, 32)[0]
    segment = struct.unpack_from('<IIQQQQQQ', blob, phoff)
    assert segment[0] == 1, 'expected one loadable ELF segment'
    base = segment[3] - segment[2]
    sites = {}
    for name, number in MARKERS.items():
        instruction = b'\x48\xb8' + struct.pack('<q', number)
        assert blob.count(instruction) == 1, f'{name}: calculation was dropped or duplicated in code'
        sites[base + blob.index(instruction)] = name
    frame = b'\x55\x53\x49\x89\xea\x48\x89\xe5\x48\x81\xec'
    assert blob.count(frame) == 1
    frame_at = blob.index(frame)
    frame_bytes = struct.unpack_from('<I', blob, frame_at + len(frame))[0]
    allocation_pc = base + frame_at + 8
    out_path, err_path = WORK / f'{case}.out', WORK / f'{case}.err'
    argv = [str(binary), *[str(v) for record in records for v in record]]
    child = os.fork()
    if child == 0:
        try:
            with out_path.open('wb') as out, err_path.open('wb') as err:
                os.dup2(out.fileno(), 1)
                os.dup2(err.fileno(), 2)
            ptrace(0)  # TRACEME; exec stops before the entry instruction.
            os.execv(binary, argv)
        except BaseException:
            os._exit(127)
    reaped = False
    counts, syscalls = collections.Counter(), collections.Counter()
    evaluations = []
    temporary_offsets = collections.defaultdict(list)
    frames, moves, steps = [], 0, 0
    pending_copy, copied_bytes, copy_operations = None, 0, 0
    try:
        while True:
            _, status = os.waitpid(child, 0)
            if os.WIFEXITED(status):
                reaped = True
                assert os.WEXITSTATUS(status) == 0, f'child exit {status}'
                break
            if os.WIFSIGNALED(status):
                reaped = True
                raise AssertionError(f'child signal {os.WTERMSIG(status)}')
            assert os.WIFSTOPPED(status) and os.WSTOPSIG(status) == signal.SIGTRAP, status
            registers = Registers()
            ptrace(12, child, ctypes.byref(registers))  # GETREGS
            pc = registers.rip
            continuing_copy = pending_copy is not None and pending_copy[0] == pc
            if pending_copy is not None:
                _, count, source, dest = pending_copy
                copied = count - registers.rcx
                assert 0 <= copied <= count, 'unexpected REP count change'
                assert registers.rsi == source + copied and registers.rdi == dest + copied
                copied_bytes += copied
                pending_copy = None
            if pc in sites:
                counts[sites[pc]] += 1
                evaluations.append(sites[pc])
            if pc == allocation_pc:
                frames.append(registers.rbp)
            if frames:
                assert registers.rsp >= frames[-1] - frame_bytes - 32, 'unaccounted scratch/stack growth'
            offset = pc - base
            instruction = blob[offset:offset + 2]
            if instruction == b'\x0f\x05':
                syscalls[registers.rax] += 1
            if instruction == b'\xf3\xa4':
                assert frames, 'copy before region reservation'
                assert not (registers.eflags & 0x400), 'copy direction flag set'
                assert frames[-1] - frame_bytes <= registers.rdi <= registers.rdi + registers.rcx <= frames[-1], 'copy escapes invocation region'
                if registers.rcx:
                    assert (registers.rsi + registers.rcx <= registers.rdi or
                            registers.rdi + registers.rcx <= registers.rsi), 'source overlaps writable destination'
                if not continuing_copy:
                    copy_operations += 1
                    source_offset = registers.rsi - base
                    for name, prefix in REUSE_PREFIXES.items():
                        if source_offset >= 0 and blob[source_offset:source_offset + len(prefix)] == prefix.encode():
                            temporary_offsets[name].append(registers.rdi - frames[-1])
                pending_copy = (pc, registers.rcx, registers.rsi, registers.rdi)
                moves += 1
            steps += 1
            assert steps < 100_000, 'instruction budget exceeded'
            ptrace(9, child)  # SINGLESTEP
    finally:
        if not reaped:
            os.kill(child, signal.SIGKILL)
            os.waitpid(child, 0)
    positive = sum(code > 0 for _, code in records)
    expected_counts = dict(eager=len(records), once=len(records), yes=positive,
                           no=len(records)-positive,
                           probe=positive if operator == 'and' else len(records)-positive)
    if {n: counts[n] for n in MARKERS} != expected_counts:
        raise EvaluationCountMismatch((counts, expected_counts))
    expected_order = []
    for _, code in records:
        expected_order.extend(['eager', 'once'])
        if (code > 0) == (operator == 'and'):
            expected_order.append('probe')
        expected_order.append('yes' if code > 0 else 'no')
    assert evaluations == expected_order, ('evaluation order', evaluations, expected_order)
    if reuse_expected is not None:
        assert set(temporary_offsets) == set(REUSE_PREFIXES), 'a dead concat was dropped'
        assert all(len(v) == len(records) for v in temporary_offsets.values()), 'a dead concat was repeated'
        for i in range(len(records)):
            addresses = {temporary_offsets[n][i] for n in REUSE_PREFIXES}
            assert len(addresses) == (1 if reuse_expected else 3), ('temporary reuse', temporary_offsets)
    assert len(frames) == len(records) and len(set(frames)) == 1, 'invocation region was not reclaimed'
    assert moves > 0, 'copy-range check was not exercised'
    assert set(syscalls) == {1, 60}, f'unexpected syscall / allocation: {syscalls}'
    expected = b''
    for title, code in records:
        piece = f'{MARKERS["once"]}{title}'
        expected += (piece * (2 if code > 0 else 1) + str(MARKERS['yes' if code > 0 else 'no']) + '\n').encode()
    assert out_path.read_bytes() == expected
    assert err_path.read_bytes() == b''
    return dict(case=case, evaluation_counts=expected_counts, evaluation_order=evaluations, frame_bytes=frame_bytes,
                region_reused=True, checked_copy_steps=moves, copied_bytes=copied_bytes,
                copy_operations=copy_operations, temporary_offsets=dict(temporary_offsets),
                syscalls=dict(syscalls), steps=steps)


reports = []
comparisons = []
for operator in ['and', 'or']:
    source = WORK / f'{operator}.verbose'
    source_text = SOURCE.replace('and probe(req)', f'{operator} probe(req)')
    if options.check_reuse:
        work = '\n'.join(f'    let discarded = concat("{prefix}", req.title)' for prefix in REUSE_PREFIXES.values())
        source_text = source_text.replace('    let unused = 420000000001', f'    let unused = 420000000001\n{work}')
        source_text = source_text.replace('reads : [req, req.code]', 'reads : [req, req.code, req.title]')
        source_text = source_text.replace('bound : 64', 'bound : 128')
    source.write_text(source_text)
    binary = WORK / operator
    subprocess.run([str(options.compiler.resolve()), str(source), '--run', 'forward',
                    '--native', str(binary)], check=True, capture_output=True)
    reference = WORK / f'{operator}-reference'
    if options.reference_compiler:
        subprocess.run([str(options.reference_compiler.resolve()), str(source), '--run', 'forward',
                        '--native', str(reference)], check=True, capture_output=True)
    for case, records in [('yes', [('é', 1)]), ('no', [('', -1)]),
                          ('repeat', [('é', 1), ('hello', -1), ('x', 2)])]:
        report = trace(binary, records, operator, f'{operator}-{case}', True if options.check_reuse else None)
        reports.append(report)
        if options.reference_compiler:
            before = trace(reference, records, operator, f'{operator}-{case}-reference', False if options.check_reuse else None)
            assert report['frame_bytes'] < before['frame_bytes'], (before, report)
            for metric in ['copied_bytes', 'copy_operations']:
                if options.check_reuse:
                    assert report[metric] == before[metric], (metric, before, report)
                else:
                    assert report[metric] < before[metric], (metric, before, report)
            comparisons.append(dict(case=report['case'], before=before, after=report))
(WORK / 'report.json').write_text(json.dumps(reports, indent=2) + '\n')
if comparisons:
    (WORK / 'comparison.json').write_text(json.dumps(comparisons, indent=2) + '\n')
    print(json.dumps(comparisons, indent=2))
print(json.dumps(reports, indent=2))
# Negative control: invert only the AND short-circuit jump. This deliberately
# runs probe on the negative input and skips it on the positive input; since
# probe repeats the same predicate, stdout remains correct in BOTH cases.
# The instruction counter must detect the semantic error that output misses.
mutant = bytearray((WORK / 'and').read_bytes())
frame = mutant.index(b'\x55\x53\x49\x89\xea\x48\x89\xe5\x48\x81\xec')
site = mutant.index(b'\x48\x85\xc0\x0f\x84', frame)
mutant[site + 4] = 0x85
bad = WORK / 'bad-short-circuit'
bad.write_bytes(mutant)
bad.chmod(0o755)
for code in [1, -1]:
    expected = subprocess.run([str(WORK / 'and'), 'é', str(code)], capture_output=True, check=True)
    actual = subprocess.run([str(bad), 'é', str(code)], capture_output=True, check=True)
    assert (actual.stdout, actual.stderr) == (expected.stdout, expected.stderr)
try:
    trace(bad, [('é', -1)], 'and', 'negative-control')
except EvaluationCountMismatch:
    print('Negative control caught: wrong short-circuit evaluation despite identical output.')
else:
    raise AssertionError('instruction counter missed the negative control')
print('All 6 instruction-trace cases passed.')
