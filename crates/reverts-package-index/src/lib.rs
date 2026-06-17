use std::collections::BTreeMap;

use reverts_ir::{AxisKind, NormalizationPassId};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackageId {
    pub name: String,
    pub version: String,
}

/// Per-package metadata carried by every candidate the package matcher
/// stores in [`FingerprintIndex`]. Held inside `Candidate::owner` so other
/// owners (cross-project module matching, future cross-bundle work) can
/// share the same index machinery without inheriting package-only fields.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackageOwner {
    pub package: PackageId,
    pub variant_path: String,
    pub external_importable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExactKey {
    pub param_count: u32,
    pub statement_count: u32,
    pub ast_hash: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CfgKey {
    pub param_count: u32,
    pub cfg_hash: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FeatureKey {
    pub param_count: u32,
    pub kind: AxisKind,
    pub hash: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StructuralKey {
    pub param_count: u32,
    pub structural_anchor: u64,
}

/// Owner abstraction used by every generic scoring helper. The matcher
/// dedups candidate sets by `(identity_key, external_function_id)`, so each
/// concrete owner type decides what counts as "the same library" for
/// deduplication purposes:
///
/// * Package owners collapse different versions of the same package into a
///   single identity, because two `pretty-bytes` versions with identical
///   helper code are unambiguous evidence — version disambiguation happens
///   later, in [`crate::PackageCandidate`]'s downstream pipeline.
/// * Cross-project module owners are dedup'd by subject module index.
/// * Future owners (cross-bundle, cross-project symbol) pick whatever
///   coarsest grouping makes a tier-unique winner meaningful.
pub trait CandidateOwner: Clone {
    type IdentityKey: Ord + Eq;

    fn identity_key(&self) -> Self::IdentityKey;
}

impl CandidateOwner for PackageOwner {
    type IdentityKey = String;

    fn identity_key(&self) -> Self::IdentityKey {
        self.package.name.clone()
    }
}

impl CandidateOwner for usize {
    type IdentityKey = Self;

    fn identity_key(&self) -> Self::IdentityKey {
        *self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Candidate<Owner> {
    pub owner: Owner,
    pub external_function_id: u64,
    pub matched_axis: AxisKind,
    pub matched_alternate: Option<NormalizationPassId>,
}

/// Concrete candidate type the package matcher uses everywhere; provided as
/// an alias so existing callers do not need to thread a type parameter.
pub type PackageCandidate = Candidate<PackageOwner>;

#[derive(Debug, Default, Clone)]
pub struct CorpusStats {
    pub axis_hash_frequencies: BTreeMap<(AxisKind, u64), u32>,
}

impl CorpusStats {
    #[must_use]
    pub fn frequency(&self, axis: AxisKind, hash: u64) -> u32 {
        *self.axis_hash_frequencies.get(&(axis, hash)).unwrap_or(&1)
    }
}

pub mod in_memory;
pub use in_memory::{FingerprintIndex, PackageFingerprintIndex};
