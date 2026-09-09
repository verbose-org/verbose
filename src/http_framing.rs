//! Bounded, close-after-one-request HTTP framing. See docs/http-bounded-io.md.
//! The native emitter uses a deterministic DFA; tests use a separate parser.

pub(crate) const DONE: u16 = 1;
pub(crate) const START: u16 = 2;
pub(crate) const DIGIT: u16 = 0x8000;
pub(crate) const LENGTH: u16 = 0x4000;
pub(crate) const TARGET: u16 = 0x2000;
pub(crate) const STATE: u16 = 0x1fff;

fn token(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// State zero rejects. Upper bits encode actions on the consumed byte.
/// Declaration-order construction makes the embedded table reproducible.
pub(crate) fn table() -> Vec<[u16; 256]> {
    fn state(t: &mut Vec<[u16; 256]>) -> u16 {
        let n = t.len() as u16;
        t.push([0; 256]);
        n
    }
    fn edge(t: &mut [[u16; 256]], s: u16, b: u8, next: u16) {
        t[s as usize][b as usize] = next;
    }
    let mut t = vec![[0; 256]; 3];
    let headers = state(&mut t);
    let end_lf = state(&mut t);
    edge(&mut t, headers, b'\r', end_lf);
    edge(&mut t, end_lf, b'\n', DONE);
    let generic_name = state(&mut t);
    let value = state(&mut t);
    let value_lf = state(&mut t);
    for b in 0..=255u8 {
        if token(b) {
            edge(&mut t, headers, b, generic_name);
            edge(&mut t, generic_name, b, generic_name);
        }
        if b == b'\t' || b >= 32 && b != 127 {
            edge(&mut t, value, b, value);
        }
    }
    edge(&mut t, generic_name, b':', value);
    edge(&mut t, value, b'\r', value_lf);
    edge(&mut t, value_lf, b'\n', headers);
    let leading = state(&mut t);
    let digits = state(&mut t);
    let trailing = state(&mut t);
    for s in [leading, trailing] {
        edge(&mut t, s, b' ', s);
        edge(&mut t, s, b'\t', s);
    }
    for b in b'0'..=b'9' {
        edge(&mut t, leading, b, digits | DIGIT);
        edge(&mut t, digits, b, digits | DIGIT);
    }
    edge(&mut t, digits, b' ', trailing);
    edge(&mut t, digits, b'\t', trailing);
    edge(&mut t, digits, b'\r', value_lf);
    edge(&mut t, trailing, b'\r', value_lf);
    // Header-name trie. Prefix mismatches rejoin the generic token scanner.
    for (name, destination) in [
        (b"content-length".as_slice(), leading | LENGTH),
        (b"transfer-encoding".as_slice(), 0),
        (b"expect".as_slice(), 0),
    ] {
        let mut s = headers;
        for &b in name {
            let existing = t[s as usize][b as usize];
            let next = if existing != generic_name && existing != 0 {
                existing
            } else {
                let n = state(&mut t);
                t[n as usize] = t[generic_name as usize];
                edge(&mut t, s, b, n);
                edge(&mut t, s, b.to_ascii_uppercase(), n);
                n
            };
            s = next;
        }
        edge(&mut t, s, b':', destination);
    }
    // A nonempty token method of at most eight bytes, then an origin target.
    let target_start = state(&mut t);
    let target = state(&mut t);
    edge(&mut t, target_start, b'/', target | TARGET);
    for b in 33..=126u8 {
        if b != b'#' {
            edge(&mut t, target, b, target | TARGET);
        }
    }
    let version = state(&mut t);
    edge(&mut t, target, b' ', version);
    let mut s = version;
    for b in b"HTTP/1." {
        let n = state(&mut t);
        edge(&mut t, s, *b, n);
        s = n;
    }
    let version_cr = state(&mut t);
    let version_lf = state(&mut t);
    edge(&mut t, s, b'0', version_cr);
    edge(&mut t, s, b'1', version_cr);
    edge(&mut t, version_cr, b'\r', version_lf);
    edge(&mut t, version_lf, b'\n', headers);
    let mut s = START;
    for count in 0..=8 {
        if count > 0 {
            edge(&mut t, s, b' ', target_start);
        }
        if count == 8 {
            break;
        }
        let n = state(&mut t);
        for b in 0..=255u8 {
            if token(b) {
                edge(&mut t, s, b, n);
            }
        }
        s = n;
    }
    t
}

#[cfg(test)]
#[derive(Debug, PartialEq)]
pub(crate) enum Frame {
    More,
    Complete(usize),
    Invalid,
}

/// Independent reference: line-based parsing, no DFA tables or action flags.
#[cfg(test)]
pub(crate) fn reference(data: &[u8], capacity: usize) -> Frame {
    use Frame::*;
    let Some(end) = data
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
    else {
        return if data.len() >= capacity {
            Invalid
        } else {
            More
        };
    };
    if end > capacity {
        return Invalid;
    }
    let mut lines = data[..end - 2].split_inclusive(|b| *b == b'\n');
    let line = lines.next().unwrap();
    let Some(line) = line.strip_suffix(b"\r\n") else {
        return Invalid;
    };
    let fields: Vec<_> = line.split(|b| *b == b' ').collect();
    if fields.len() != 3
        || fields[0].is_empty()
        || fields[0].len() > 8
        || !fields[0].iter().all(|b| token(*b))
        || !fields[1].starts_with(b"/")
        || fields[1].len() > 256
        || !fields[1]
            .iter()
            .all(|b| (33..=126).contains(b) && *b != b'#')
        || ![b"HTTP/1.0".as_slice(), b"HTTP/1.1".as_slice()].contains(&fields[2])
    {
        return Invalid;
    }
    let mut length = None;
    for line in lines {
        let Some(line) = line.strip_suffix(b"\r\n") else {
            return Invalid;
        };
        let Some(colon) = line.iter().position(|b| *b == b':') else {
            return Invalid;
        };
        let (name, value) = (&line[..colon], &line[colon + 1..]);
        if name.is_empty()
            || !name.iter().all(|b| token(*b))
            || !value.iter().all(|b| *b == b'\t' || *b >= 32 && *b != 127)
        {
            return Invalid;
        }
        if name.eq_ignore_ascii_case(b"transfer-encoding") || name.eq_ignore_ascii_case(b"expect") {
            return Invalid;
        }
        if name.eq_ignore_ascii_case(b"content-length") {
            if length.is_some() {
                return Invalid;
            }
            let value = value.trim_ascii();
            if value.is_empty() {
                return Invalid;
            }
            let mut n = 0usize;
            for &b in value {
                if !b.is_ascii_digit() {
                    return Invalid;
                }
                let Some(next) = n
                    .checked_mul(10)
                    .and_then(|n| n.checked_add((b - b'0') as usize))
                else {
                    return Invalid;
                };
                if next > capacity {
                    return Invalid;
                }
                n = next;
            }
            length = Some(n);
        }
    }
    let total = end + length.unwrap_or(0);
    if total > capacity {
        Invalid
    } else if data.len() < total {
        More
    } else {
        Complete(total)
    }
}
