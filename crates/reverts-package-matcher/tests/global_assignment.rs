//! L7 Hungarian global assignment correctness fixture per design spec §12.
//!
//! Builds synthetic 10-function bundles where the strict exact candidate set
//! still needs Hungarian one-to-one assignment to avoid reusing package
//! functions.
//!
//! Because pkg_a covers the first 5 slots via primary-exact and pkg_b covers
//! the last 5 slots via primary-exact, the optimal split is deterministically
//! 5 to pkg_a and 5 to pkg_b.

use reverts_ir::{
    AxisHashes, AxisKind, ByteRange, FunctionFingerprint, FunctionId, ModuleId, NormalizationPassId,
};
use reverts_package_index::{Candidate, ExactKey, FingerprintIndex, PackageId, PackageOwner};
use reverts_package_matcher::assign_globally;

fn zero_axes(ast: u64) -> AxisHashes {
    AxisHashes {
        ast,
        cfg: 0,
        normalized_cfg: 0,
        return_pattern: 0,
        effect_pattern: 0,
        literal_anchor: None,
        access_pattern: None,
        structural_anchor: 0,
        literal_shape: None,
        access_shape: None,
        expression_shape: None,
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
        alternates: vec![reverts_ir::AlternateAxisHashes {
            pass: NormalizationPassId::TsRuntimeErased,
            statement_count: 1,
            axes: zero_axes(ALT_BASE + slot),
        }],
    }
}

/// Base AST hash for primary fingerprints (must not overlap with ALT_BASE range).
const PRIMARY_BASE: u64 = 10_000;
/// Base AST hash for alternate fingerprints (must not overlap with PRIMARY_BASE range).
const ALT_BASE: u64 = 20_000;

#[test]
fn hungarian_assigns_chunk_optimally_when_two_packages_share_helpers() {
    let mut idx = FingerprintIndex::new();
    let pkg_a = PackageId {
        name: "a".into(),
        version: "1.0".into(),
    };
    let pkg_b = PackageId {
        name: "b".into(),
        version: "1.0".into(),
    };

    // Slot layout:
    //   Slots 0..5  → pkg_a is unique at PRIMARY_BASE+slot.
    //   Slots 5..10 → pkg_b is unique at PRIMARY_BASE+slot.
    // Alternate hashes are present in the index and bundle fingerprints but
    // are intentionally outside the strict exact pipeline.
    for slot in 0..10u64 {
        let primary_hash = PRIMARY_BASE + slot;
        let alt_hash = ALT_BASE + slot;

        // Each pkg has a UNIQUE external_function_id per slot.
        let pkg_a_fn_id = slot;
        let pkg_b_fn_id = 100 + slot;

        if slot < 5 {
            // Slots 0-4: pkg_a appears at the primary hash.
            idx.insert_exact(
                ExactKey {
                    param_count: 1,
                    statement_count: 1,
                    ast_hash: primary_hash,
                },
                Candidate {
                    owner: PackageOwner {
                        package: pkg_a.clone(),
                        variant_path: "a/i.js".into(),
                        external_importable: true,
                    },
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
                    owner: PackageOwner {
                        package: pkg_b.clone(),
                        variant_path: "b/i.js".into(),
                        external_importable: true,
                    },
                    external_function_id: pkg_b_fn_id,
                    matched_axis: AxisKind::Ast,
                    matched_alternate: None,
                },
            );
        } else {
            // Slots 5-9: pkg_b appears at the primary hash.
            idx.insert_exact(
                ExactKey {
                    param_count: 1,
                    statement_count: 1,
                    ast_hash: primary_hash,
                },
                Candidate {
                    owner: PackageOwner {
                        package: pkg_b.clone(),
                        variant_path: "b/i.js".into(),
                        external_importable: true,
                    },
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
                    owner: PackageOwner {
                        package: pkg_a.clone(),
                        variant_path: "a/i.js".into(),
                        external_importable: true,
                    },
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
        .filter(|a| a.chosen_package_name() == Some("a"))
        .count();
    let count_b = assignments
        .iter()
        .filter(|a| a.chosen_package_name() == Some("b"))
        .count();

    assert_eq!(count_a + count_b, 10, "all assignments must land in a or b");

    // Slots 0-4 match pkg_a and slots 5-9 match pkg_b.
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

/// Verify that reused primary-exact candidate columns are assigned only once
/// while exact-alternate candidates can safely fill the remaining rows at a
/// lower tier weight.
#[test]
fn hungarian_assigns_reused_exact_columns_once() {
    let mut idx = FingerprintIndex::new();
    let pkg_a = PackageId {
        name: "x".into(),
        version: "1.0".into(),
    };
    let pkg_b = PackageId {
        name: "y".into(),
        version: "1.0".into(),
    };

    // Each primary exact hash maps to one of five reused pkg_x functions.
    // Matching the same package function twice is forbidden by the assignment.
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
                owner: PackageOwner {
                    package: pkg_a.clone(),
                    variant_path: "x/i.js".into(),
                    external_importable: true,
                },
                external_function_id: a_fn_id,
                matched_axis: AxisKind::Ast,
                matched_alternate: None,
            },
        );
        // Slot's alternate hash → pkg_y candidate. It is available as a lower
        // exact-alternate tier, so it can fill rows whose reused primary pkg_x
        // column has already been consumed.
        idx.insert_exact(
            ExactKey {
                param_count: 1,
                statement_count: 1,
                ast_hash: alt_hash,
            },
            Candidate {
                owner: PackageOwner {
                    package: pkg_b.clone(),
                    variant_path: "y/i.js".into(),
                    external_importable: true,
                },
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
            alternates: vec![reverts_ir::AlternateAxisHashes {
                pass: NormalizationPassId::TsRuntimeErased,
                statement_count: 1,
                axes: zero_axes(40_000 + slot),
            }],
        })
        .collect();

    let assignments = assign_globally(&fps, &idx);

    assert_eq!(
        assignments.len(),
        10,
        "expected 10 assignments, got {}",
        assignments.len()
    );
    let chosen_count = assignments.iter().filter(|a| a.chosen.is_some()).count();
    let count_a = assignments
        .iter()
        .filter(|a| a.chosen_package_name() == Some("x"))
        .count();
    let count_b = assignments
        .iter()
        .filter(|a| a.chosen_package_name() == Some("y"))
        .count();
    assert_eq!(chosen_count, 10);
    assert_eq!(count_a, 5);
    assert_eq!(count_b, 5);
}
