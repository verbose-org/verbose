"""Conservative source snapshots and execution-only edits; no language parser.

The real compiler verifies the baseline and every edited program. Unknown
layouts refuse here; strings and comments never become configuration lines.
"""
import re
from pathlib import Path

IDENT = r'[A-Za-z_][A-Za-z_0-9]*'
CONFIG = ('mode', 'native_stack', 'native_memory', 'max_in_flight', 'result_batch')


def masked(text):
    """Keep offsets/newlines, hide comments and literal contents, retain quote starts."""
    result = list(text)
    strings = {}
    i = 0
    while i < len(text):
        if text.startswith('--', i):
            end = text.find('\n', i)
            end = len(text) if end < 0 else end
        elif text[i] == '"':
            end = i + 1
            while end < len(text) and text[end] != '"':
                end += 2 if text[end] == '\\' else 1
            if end >= len(text):
                raise ValueError('source snapshot: unterminated string')
            strings[i] = text[i + 1:end]
            end += 1
        else:
            i += 1
            continue
        for at in range(i, end):
            if text[at] != '\n':
                result[at] = ' '
        if i in strings:
            result[i] = '"'
        i = end
    return ''.join(result), strings


def relative(value):
    if not isinstance(value, str) or not value or any(c in value for c in '\\\x00\r\n'):
        raise ValueError('snapshot paths must be nonempty unescaped relative paths')
    path = Path(value)
    if path.is_absolute() or '..' in path.parts:
        raise ValueError(f'snapshot path escapes its root: {value}')
    return path


def snapshot(entry):
    """Follow compiler import semantics: uses are relative to the entry directory."""
    root = entry.parent
    files, parsed = {}, set()
    pending = [(Path(entry.name), True)]
    while pending:
        name, is_source = pending.pop()
        relative(str(name))
        path = root
        for part in name.parts:
            path /= part
            if path.is_symlink():
                raise ValueError(f'snapshot refuses symlink: {path}')
        if name not in files:
            files[name] = path.read_bytes()
        if not is_source or name in parsed:
            continue
        parsed.add(name)
        text = files[name].decode('utf-8')
        visible, strings = masked(text)
        for m in re.finditer(r'(?m)^use\b[^\n]*', visible):
            use = re.fullmatch(r'use +(" *)', m[0].rstrip())
            if not use:
                raise ValueError('snapshot requires use "relative/path" on one line')
            start = m.start() + use.start(1)
            pending.append((relative(strings[start]), True))
        for m in re.finditer(r'(?m)^ +@source\b[^\n]*', visible):
            ref = re.fullmatch(r' +@source *: *(" *|[A-Za-z_][A-Za-z_0-9.]*) *: *[0-9]+ *', m[0])
            if not ref:
                raise ValueError('snapshot requires a complete @source reference on one line')
            start = m.start() + ref.start(1)
            value = strings[start] if start in strings else ref[1].strip()
            pending.append((name.parent / relative(value), False))
    return files


def configuration(text, execution):
    if '\r' in text or not text.endswith('\n'):
        raise ValueError('source editor requires LF lines and a final newline')
    visible, _ = masked(text)
    headers = list(re.finditer(r'(?m)^execution +(' + IDENT + r') *$', visible))
    found = [h for h in headers if h[1] == execution]
    if len(found) != 1:
        raise ValueError('selected execution must occur exactly once in the entry file')
    begin = visible.find('\n', found[0].end()) + 1
    tail = re.search(r'(?m)^\S', visible[begin:])
    end = begin + tail.start() if tail else len(visible)
    body = visible[begin:end]
    nonempty = [line for line in body.splitlines() if line.strip()]
    indent = min(len(line) - len(line.lstrip(' ')) for line in nonempty)
    spans, values = {}, {}
    at = begin
    for line in body.splitlines(keepends=True):
        m = re.fullmatch(r' {' + str(indent) + r'}(' + '|'.join(CONFIG) + r') *: *(\w+) *\n', line)
        if m:
            if m[1] in spans:
                raise ValueError(f'duplicate execution configuration: {m[1]}')
            spans[m[1]] = (at, at + len(line))
            values[m[1]] = m[2] if m[1] == 'mode' else int(m[2])
        elif re.match(r' *(' + '|'.join(CONFIG) + r')\b', line):
            raise ValueError('source editor requires complete scalar configuration lines')
        at += len(line)
    if 'mode' not in values:
        raise ValueError('source editor could not identify execution mode')
    required = {'mode', 'native_stack'} if values['mode'] == 'sequential' else {'mode', 'max_in_flight', 'native_memory'}
    if not required <= values.keys():
        raise ValueError('baseline requires an explicit native ceiling and complete configuration')
    return values, spans, indent


def variant_source(text, execution, variant, budgets):
    values, spans, indent = configuration(text, execution)
    ceilings = {k: values[k] for k in ('native_stack', 'native_memory') if k in values}
    if ceilings.keys() & budgets.keys():
        raise ValueError('experiment budgets cannot override a source ceiling')
    ceilings.update(budgets)
    key = 'native_stack' if variant['mode'] == 'sequential' else 'native_memory'
    if key not in ceilings:
        raise ValueError(f'{variant["name"]} requires explicit {key} for its resource scope')
    new = {'mode': variant['mode'], key: ceilings[key]}
    if variant['mode'] == 'concurrent':
        new.update(max_in_flight=variant['max_in_flight'], result_batch=variant['result_batch'])
    if new['mode'] == values['mode']:
        # Keep unchanged declarations, spacing and comments in place. A normal
        # batching proposal should be a one-value diff, not a reformatted block.
        replacements = []
        for key, value in new.items():
            if values.get(key) == value:
                continue
            if key in spans:
                start, end = spans[key]
                content = re.sub(r'(: *)\w+', lambda m: m[1] + str(value), text[start:end], count=1)
            else:
                start = end = spans['mode'][1]
                content = f'{" " * indent}{key}: {value}\n'
            replacements.append((start, end, content))
        for start, end, content in sorted(replacements, reverse=True):
            text = text[:start] + content + text[end:]
        return text
    # Replace the mode line and remove only the other identified config lines.
    replacements = []
    for key, (start, end) in spans.items():
        content = ''.join(f'{" " * indent}{k}: {v}\n' for k, v in new.items()) if key == 'mode' else ''
        replacements.append((start, end, content))
    for start, end, content in sorted(replacements, reverse=True):
        text = text[:start] + content + text[end:]
    return text
