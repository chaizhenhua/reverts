use crate::ModuleId;

/// Half-open byte range `[start, end)` over a source file: `start` is
/// inclusive, `end` is exclusive. Touching ranges (e.g. `[10, 20)` and
/// `[20, 30)`) do not overlap and do not contain each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteRange {
    pub start: u32,
    pub end: u32,
}

impl ByteRange {
    #[must_use]
    pub const fn new(start: u32, end: u32) -> Self {
        debug_assert!(start <= end, "ByteRange requires start <= end");
        Self { start, end }
    }

    #[must_use]
    pub const fn contains(&self, other: Self) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    #[must_use]
    pub const fn overlaps(&self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FunctionId {
    pub module_id: ModuleId,
    pub span: ByteRange,
}

impl FunctionId {
    #[must_use]
    pub const fn new(module_id: ModuleId, span: ByteRange) -> Self {
        Self { module_id, span }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_range_contains_subspan_but_not_overlap_with_disjoint() {
        let outer = ByteRange::new(10, 30);
        let inner = ByteRange::new(15, 20);
        let disjoint = ByteRange::new(40, 50);

        assert!(outer.contains(inner));
        assert!(!outer.contains(disjoint));
        assert!(outer.overlaps(inner));
        assert!(!outer.overlaps(disjoint));
    }

    #[test]
    fn byte_range_touching_boundaries_are_disjoint() {
        let left = ByteRange::new(10, 20);
        let right = ByteRange::new(20, 30);

        assert!(!left.overlaps(right));
        assert!(!right.overlaps(left));
        assert!(!left.contains(right));
        assert!(!right.contains(left));
    }

    #[test]
    fn function_id_pairs_module_and_span() {
        let id = FunctionId::new(ModuleId(7), ByteRange::new(0, 42));

        assert_eq!(id.module_id, ModuleId(7));
        assert_eq!(id.span.start, 0);
        assert_eq!(id.span.end, 42);
    }
}
