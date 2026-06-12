pub const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
pub const FNV_PRIME: u64 = 0x0100_0000_01b3;

#[must_use]
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, bytes);
    hash
}

pub fn update_fnv1a(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}

#[must_use]
pub fn fnv1a_hex(bytes: &[u8]) -> String {
    format!("{:016x}", fnv1a(bytes))
}

/// FNV-1a digest of an ordered set of string tokens, prefixed with `tag`.
///
/// Returns `None` when the set is empty so that axis hashers can signal
/// "this axis carries no evidence for this function" without a sentinel
/// zero hash. The iteration order of the input matters: pass a
/// `BTreeSet`/sorted iterator if the caller wants order-independence.
///
/// This is the canonical builder for the "bag-of-strings" axes
/// (`callee_set`, `throw_set`, `literal_anchor`, `access_pattern`,
/// `access_shape`) — every such axis would otherwise inline the same
/// nine-line emission block.
#[must_use]
pub fn fnv1a_of_string_set<'a, I>(items: I, tag: &[u8]) -> Option<u64>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut iter = items.into_iter().peekable();
    iter.peek()?;
    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, tag);
    for item in iter {
        update_fnv1a(&mut hash, item.as_bytes());
        update_fnv1a(&mut hash, b"|");
    }
    Some(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn fnv1a_is_deterministic_and_distinguishes_inputs() {
        assert_eq!(fnv1a(b"hello"), fnv1a(b"hello"));
        assert_ne!(fnv1a(b"hello"), fnv1a(b"world"));
    }

    #[test]
    fn update_fnv1a_matches_one_shot() {
        let mut accum = FNV_OFFSET_BASIS;
        update_fnv1a(&mut accum, b"foo");
        update_fnv1a(&mut accum, b"bar");
        assert_eq!(accum, fnv1a(b"foobar"));
    }

    #[test]
    fn fnv1a_of_string_set_returns_none_on_empty() {
        let empty: BTreeSet<String> = BTreeSet::new();
        assert!(fnv1a_of_string_set(empty.iter().map(String::as_str), b"tag|").is_none());
    }

    #[test]
    fn fnv1a_of_string_set_differs_per_tag() {
        let mut set: BTreeSet<String> = BTreeSet::new();
        set.insert("a".into());
        set.insert("b".into());
        let one = fnv1a_of_string_set(set.iter().map(String::as_str), b"foo|");
        let two = fnv1a_of_string_set(set.iter().map(String::as_str), b"bar|");
        assert!(one.is_some());
        assert_ne!(one, two, "different tags must produce different hashes");
    }

    #[test]
    fn fnv1a_of_string_set_is_order_sensitive_per_input_iteration() {
        // The helper does not sort internally; callers control order.
        // BTreeSet iteration is sorted ascending, so identical sets hash
        // identically — verified here.
        let mut a: BTreeSet<String> = BTreeSet::new();
        a.insert("y".into());
        a.insert("x".into());
        let mut b: BTreeSet<String> = BTreeSet::new();
        b.insert("x".into());
        b.insert("y".into());
        assert_eq!(
            fnv1a_of_string_set(a.iter().map(String::as_str), b"t|"),
            fnv1a_of_string_set(b.iter().map(String::as_str), b"t|"),
        );
    }
}
