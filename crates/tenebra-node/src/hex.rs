//! Lowercase hex helpers.

pub fn encode(b: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(D[(x >> 4) as usize] as char);
        s.push(D[(x & 15) as usize] as char);
    }
    s
}

pub fn decode(s: &str) -> Option<Vec<u8>> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if s.len() % 2 != 0 {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    s.as_bytes()
        .chunks(2)
        .map(|p| Some(nib(p[0])? << 4 | nib(p[1])?))
        .collect()
}

pub fn decode32(s: &str) -> Option<[u8; 32]> {
    decode(s)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let b = [0u8, 1, 0xab, 0xff];
        assert_eq!(super::decode(&super::encode(&b)).unwrap(), b);
        assert_eq!(super::decode("0xAB"), Some(vec![0xab]));
        assert_eq!(super::decode("abc"), None);
        assert_eq!(super::decode("zz"), None);
    }
}
