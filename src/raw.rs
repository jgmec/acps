//! Schema-less protobuf dump, for inspecting fields of Apple's replies that
//! src/apple.proto doesn't describe.

use std::collections::BTreeMap;

enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed64(&'a [u8]),
    Fixed32(&'a [u8]),
}

fn read_varint(buf: &[u8], i: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let b = *buf.get(*i)?;
        *i += 1;
        value |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// Every top-level field of `buf`, or None if it isn't a valid protobuf message.
fn parse(buf: &[u8]) -> Option<Vec<(u64, Value<'_>)>> {
    let mut i = 0;
    let mut out = vec![];
    while i < buf.len() {
        let key = read_varint(buf, &mut i)?;
        let (field, wire_type) = (key >> 3, key & 7);
        if field == 0 {
            return None;
        }
        let value = match wire_type {
            0 => Value::Varint(read_varint(buf, &mut i)?),
            1 => Value::Fixed64(take(buf, &mut i, 8)?),
            2 => {
                let len = usize::try_from(read_varint(buf, &mut i)?).ok()?;
                Value::Bytes(take(buf, &mut i, len)?)
            }
            5 => Value::Fixed32(take(buf, &mut i, 4)?),
            _ => return None,
        };
        out.push((field, value));
    }
    Some(out)
}

fn take<'a>(buf: &'a [u8], i: &mut usize, n: usize) -> Option<&'a [u8]> {
    let v = buf.get(*i..i.checked_add(n)?)?;
    *i += n;
    Some(v)
}

#[derive(Default)]
struct FieldStats {
    count: usize,
    nested: bool,
    values: BTreeMap<String, usize>,
}

fn walk(buf: &[u8], path: &mut Vec<u64>, depth: usize, stats: &mut BTreeMap<Vec<u64>, FieldStats>) {
    for (field, value) in parse(buf).unwrap_or_default() {
        path.push(field);
        let entry = stats.entry(path.clone()).or_default();
        entry.count += 1;
        let shown = match value {
            // nested messages are walked; anything else that's length-delimited is shown as hex
            Value::Bytes(b) if depth < 4 && !b.is_empty() && parse(b).is_some() => {
                entry.nested = true;
                walk(b, path, depth + 1, stats);
                None
            }
            // varints shown as signed: Apple uses -1 for "unknown"
            Value::Varint(v) => Some((v as i64).to_string()),
            Value::Bytes(b) | Value::Fixed64(b) | Value::Fixed32(b) => {
                Some(b.iter().take(24).map(|x| format!("{x:02x}")).collect())
            }
        };
        if let Some(shown) = shown {
            *stats.get_mut(path.as_slice()).unwrap().values.entry(shown).or_default() += 1;
        }
        path.pop();
    }
}

/// Summary of every field path in a raw reply (10-byte header, then protobuf): how often
/// it occurs, how many distinct values it has, and the most common values.
pub fn summarize(body: &[u8]) -> String {
    let mut out = String::new();
    let header: Vec<String> = body.iter().take(10).map(|b| format!("{b:02x}")).collect();
    let declared = body.get(8..10).map_or(0, |b| u16::from_be_bytes([b[0], b[1]]));
    out += &format!(
        "reply: {} bytes; header {} (declares {} bytes, {} follow)\n\n",
        body.len(),
        header.join(" "),
        declared,
        body.len().saturating_sub(10)
    );

    let mut stats = BTreeMap::new();
    walk(body.get(10..).unwrap_or_default(), &mut vec![], 0, &mut stats);
    out += &format!("{:<16} {:>6} {:>9}  most common values (value: times)\n", "field", "count", "distinct");
    for (path, s) in &stats {
        let name = path.iter().map(u64::to_string).collect::<Vec<_>>().join(".");
        let values = if s.nested && s.values.is_empty() {
            "(message)".to_string()
        } else {
            let mut common: Vec<_> = s.values.iter().collect();
            common.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
            common.iter().take(4).map(|(v, n)| format!("{v}: {n}")).collect::<Vec<_>>().join(", ")
        };
        let distinct = if s.values.is_empty() { String::new() } else { s.values.len().to_string() };
        out += &format!("{name:<16} {:>6} {distinct:>9}  {values}\n", s.count);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints() {
        let mut i = 0;
        assert_eq!(read_varint(&[0xac, 0x02], &mut i), Some(300));
        assert_eq!(i, 2);
        // -1 as a 64-bit varint
        let mut minus_one = vec![0xff; 9];
        minus_one.push(0x01);
        assert_eq!(read_varint(&minus_one, &mut 0).map(|v| v as i64), Some(-1));
        // truncated
        assert_eq!(read_varint(&[0x80], &mut 0), None);
    }

    #[test]
    fn parse_rejects_what_isnt_protobuf() {
        // field 0 doesn't exist
        assert!(parse(&[0x00, 0x01]).is_none());
        // length past the end
        assert!(parse(&[0x0a, 0x05, 0x01]).is_none());
        // wire type 3 (groups) isn't used
        assert!(parse(&[0x0b]).is_none());
        assert_eq!(parse(&[]).map(|f| f.len()), Some(0));
    }

    #[test]
    fn summarize_counts_nested_fields() {
        // field 22 (a cell) twice, each with field 1 = 284 and field 3 = -1
        let cell = [0x08, 0x9c, 0x02, 0x18, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
        let mut message = vec![];
        for _ in 0..2 {
            message.extend([0xb2, 0x01, cell.len() as u8]);
            message.extend(cell);
        }
        let mut body = vec![0, 1, 0, 0, 0, 1, 0, 0];
        body.extend((message.len() as u16).to_be_bytes());
        body.extend(&message);

        let summary = summarize(&body);
        assert!(summary.contains(&format!("declares {} bytes, {} follow", message.len(), message.len())));
        let line = |field: &str| summary.lines().find(|l| l.starts_with(&format!("{field} "))).unwrap().to_string();
        assert!(line("22").contains("(message)"), "{summary}");
        assert!(line("22.1").contains("284: 2"), "{summary}");
        assert!(line("22.3").contains("-1: 2"), "{summary}");
    }

    #[test]
    fn summarize_survives_short_replies() {
        assert!(summarize(&[1, 2, 3]).starts_with("reply: 3 bytes"));
    }
}
