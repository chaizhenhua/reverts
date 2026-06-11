//! L7 Hungarian global assignment correctness fixture per design spec §12.
//!
//! Builds a synthetic 10-function bundle where each function has exactly TWO
//! candidates — one from pkg_a (via primary exact tier) and one from pkg_b
//! (via exact-alternate tier). Greedy tier-ordering would naively prefer the
//! primary-exact candidate for every function; Hungarian's globally-optimal
//! assignment should produce a valid complete assignment of all 10 functions.
//!
//! Because pkg_a covers the first 5 slots via primary-exact and pkg_b covers
//! the last 5 slots via primary-exact (with crossed alternates), the optimal
//! assignment is deterministically 5 to pkg_a and 5 to pkg_b.

use reverts_ir::{
    AxisHashes, AxisKind, ByteRange, FunctionFingerprint, FunctionId, ModuleId, NormalizationPassId,
};
use reverts_package_index::{Candidate, ExactKey, InMemoryFingerprintIndex, PackageId};
use reverts_package_matcher::cascade::assign_globally;

fn zero_axes(ast: u64) -> AxisHashes {
    AxisHashes {
        ast,
        cfg: 0,
        return_pattern: 0,
        effect_pattern: 0,
        literal_anchor: None,
        access_pattern: None,
        structural_anchor: 0,
        literal_shape: None,
        access_shape: None,
        callee_set: None,
        binding_pattern: 0,
        throw_set: None,
    }
}

/// Construct a bundle fingerprint for the given slot.
///
/// - primary.ast = PRIMARY_BASE + slot
/// - alternates = [(TsRuntimeErased, {ast: ALT_BASE + slot})]
fn bundle_fp(slot: u64) -> FunctionFingerprint {
    FunctionFingerprint {
        id: FunctionId::new(
            ModuleId(1),
            ByteRange::new(slot as u32 * 10, slot as u32 * 10 + 5),
        ),
        param_count: 1,
        statement_count: 1,
        primary: zero_axes(PRIMARY_BASE + slot),
        alternates: vec![(
            NormalizationPassId::TsRuntimeErased,
            zero_axes(ALT_BASE + slot),
        )],
    }
}

/// Base AST hash for primary fingerprints (must not overlap with ALT_BASE range).
const PRIMARY_BASE: u64 = 10_000;
/// Base AST hash for alternate fingerprints (must not overlap with PRIMARY_BASE range).
const ALT_BASE: u64 = 20_000;

#[test]
fn hungarian_assigns_chunk_optimally_when_two_packages_share_helpers() {
    let mut idx = InMemoryFingerprintIndex::new();
    let pkg_a = PackageId {
        name: "a".into(),
        version: "1.0".into(),
    };
    let pkg_b = PackageId {
        name: "b".into(),
        version: "1.0".into(),
    };

    // Slot layout:
    //   Slots 0..5  → pkg_a is unique at PRIMARY_BASE+slot;
    //                  pkg_b is unique at ALT_BASE+slot.
    //   Slots 5..10 → pkg_b is unique at PRIMARY_BASE+slot;
    //                  pkg_a is unique at ALT_BASE+slot.
    //
    // Result: slots 0..5 prefer pkg_a (exact > exact-alternate in weight),
    //         slots 5..10 prefer pkg_b. Hungarian assigns 5 to each.
    for slot in 0..10u64 {
        let primary_hash = PRIMARY_BASE + slot;
        let alt_hash = ALT_BASE + slot;

        // Each pkg has a UNIQUE external_function_id per slot.
        let pkg_a_fn_id = slot;
        let pkg_b_fn_id = 100 + slot;

        if slot < 5 {
            // Slots 0-4: pkg_a appears at primary hash (try_exact → rank 0)
            //            pkg_b appears at alt hash     (try_exact_alternate → rank 1)
            idx.insert_exact(
                ExactKey {
                    param_count: 1,
                    statement_count: 1,
                    ast_hash: primary_hash,
                },
                Candidate {
                    package: pkg_a.clone(),
                    variant_path: "a/i.js".into(),
                    external_function_id: pkg_a_fn_id,
                    matched_axis: AxisKind::Ast,
                    matched_alternate: None,
                },
            );
            idx.insert_exact(
                ExactKey {
                    param_count: 1,
                    statement_count: 1,
                    ast_hash: alt_hash,
                },
                Candidate {
                    package: pkg_b.clone(),
                    variant_path: "b/i.js".into(),
                    external_function_id: pkg_b_fn_id,
                    matched_axis: AxisKind::Ast,
                    matched_alternate: None,
                },
            );
        } else {
            // Slots 5-9: pkg_b appears at primary hash (try_exact → rank 0)
            //            pkg_a appears at alt hash     (try_exact_alternate → rank 1)
            idx.insert_exact(
                ExactKey {
                    param_count: 1,
                    statement_count: 1,
                    ast_hash: primary_hash,
                },
                Candidate {
                    package: pkg_b.clone(),
                    variant_path: "b/i.js".into(),
                    external_function_id: pkg_b_fn_id,
                    matched_axis: AxisKind::Ast,
                    matched_alternate: None,
                },
            );
            idx.insert_exact(
                ExactKey {
                    param_count: 1,
                    statement_count: 1,
                    ast_hash: alt_hash,
                },
                Candidate {
                    package: pkg_a.clone(),
                    variant_path: "a/i.js".into(),
                    external_function_id: pkg_a_fn_id,
                    matched_axis: AxisKind::Ast,
                    matched_alternate: None,
                },
            );
        }
    }

    // Build 10 bundle fps.
    let fps: Vec<FunctionFingerprint> = (0..10u64).map(bundle_fp).collect();

    let assignments = assign_globally(&fps, &idx);

    // 10 bundle functions must each be assigned.
    assert_eq!(
        assignments.len(),
        10,
        "expected 10 assignments, got {}",
        assignments.len()
    );

    let count_a = assignments
        .iter()
        .filter(|a| a.chosen.as_ref().map(|m| m.candidate.package.name.as_str()) == Some("a"))
        .count();
    let count_b = assignments
        .iter()
        .filter(|a| a.chosen.as_ref().map(|m| m.candidate.package.name.as_str()) == Some("b"))
        .count();

    assert_eq!(count_a + count_b, 10, "all assignments must land in a or b");

    // Slots 0-4 prefer pkg_a (primary-exact, rank 0, higher weight).
    // Slots 5-9 prefer pkg_b (primary-exact, rank 0, higher weight).
    // Because each (package, external_function_id) is unique and the
    // Hungarian assignment maximises total weight, the optimal split is 5/5.
    assert_eq!(
        count_a, 5,
        "expected 5 assignments to pkg_a (got {count_a}); pkg_b got {count_b}"
    );
    assert_eq!(
        count_b, 5,
        "expected 5 assignments to pkg_b (got {count_b}); pkg_a got {count_a}"
    );
}

/// Verify that when both packages have equal-weight candidates for every
/// bundle function AND each package has exactly 5 unique functions (shared
/// across all 10 slots), the Hungarian assignment still produces a complete
/// 10-assignment result — at least one match per package.
#[test]
fn hungarian_assigns_all_fps_when_candidates_overlap() {
    let mut idx = InMemoryFingerprintIndex::new();
    let pkg_a = PackageId {
        name: "x".into(),
        version: "1.0".into(),
    };
    let pkg_b = PackageId {
        name: "y".into(),
        version: "1.0".into(),
    };

    // 5 pkg_a functions (fn 0..4), each covering ALL 10 slots via structural key,
    // and 5 pkg_b functions (fn 5..9), each covering ALL 10 slots.
    // The structural-only tier for each fp sees multiple candidates → rejects.
    // Instead, use the exact tier with unique (per-slot) hashes; each slot maps
    // to one pkg_a and one pkg_b function.
    for slot in 0..10u64 {
        let a_fn_id = slot % 5; // 5 unique pkg_a functions, reused across slots
        let b_fn_id = 5 + (slot % 5); // 5 unique pkg_b functions, reused across slots
        let primary_hash = 30_000 + slot;
        let alt_hash = 40_000 + slot;

        // Slot's primary hash → pkg_a candidate (unique per slot)
        idx.insert_exact(
            ExactKey {
                param_count: 1,
                statement_count: 1,
                ast_hash: primary_hash,
            },
            Candidate {
                package: pkg_a.clone(),
                variant_path: "x/i.js".into(),
                external_function_id: a_fn_id,
                matched_axis: AxisKind::Ast,
                matched_alternate: None,
            },
        );
        // Slot's alt hash → pkg_b candidate (unique per slot)
        idx.insert_exact(
            ExactKey {
                param_count: 1,
                statement_count: 1,
                ast_hash: alt_hash,
            },
            Candidate {
                package: pkg_b.clone(),
                variant_path: "y/i.js".into(),
                external_function_id: b_fn_id,
                matched_axis: AxisKind::Ast,
                matched_alternate: None,
            },
        );
    }

    let fps: Vec<FunctionFingerprint> = (0..10u64)
        .map(|slot| FunctionFingerprint {
            id: FunctionId::new(
                ModuleId(2),
                ByteRange::new(slot as u32 * 10, slot as u32 * 10 + 5),
            ),
            param_count: 1,
            statement_count: 1,
            primary: zero_axes(30_000 + slot),
            alternates: vec![(
                NormalizationPassId::TsRuntimeErased,
                zero_axes(40_000 + slot),
            )],
        })
        .collect();

    let assignments = assign_globally(&fps, &idx);

    // With 5 unique pkg_a functions and 5 unique pkg_b functions across 10 slots,
    // Hungarian assigns at most 5 to pkg_a and at most 5 to pkg_b (uniqueness
    // constraint). The globally-optimal assignment uses all 10 unique columns →
    // exactly 5 to pkg_a and 5 to pkg_b.
    assert_eq!(
        assignments.len(),
        10,
        "expected 10 assignments, got {}",
        assignments.len()
    );
    let count_a = assignments
        .iter()
        .filter(|a| a.chosen.as_ref().map(|m| m.candidate.package.name.as_str()) == Some("x"))
        .count();
    let count_b = assignments
        .iter()
        .filter(|a| a.chosen.as_ref().map(|m| m.candidate.package.name.as_str()) == Some("y"))
        .count();
    assert_eq!(count_a + count_b, 10);
    assert!(count_a >= 1, "pkg_x must receive at least one match");
    assert!(count_b >= 1, "pkg_y must receive at least one match");
}
