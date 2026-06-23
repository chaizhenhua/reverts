#!/usr/bin/env python3
"""Extract the gold answer-key from a heavily-named decompile DB (v1).

The gold is the curated naming + package classification that we treat as the
reference. A fresh run of the current SKILL is later scored against it by
score.py.

Join keys (all bundle-stable, verified across v1<->v2):
  - clusters : island_cluster_names.fingerprint
  - vendored : modules.original_name / package_attributions.module_original_name (node_modules path)
  - bindings : (file_path, original_name, binding_key)   [file_path may drift on relocated modules]
  - island   : (binding_name, function_span_start, function_span_end)
"""
import argparse, json, os, sqlite3, sys


def q(con, sql, args=()):
    cur = con.execute(sql, args)
    cols = [c[0] for c in cur.description]
    return [dict(zip(cols, r)) for r in cur.fetchall()]


def extract(db_path, out_dir):
    con = sqlite3.connect(db_path)

    # ---- bindings: keep only REAL semantic names (drop mechanical renamed_*) ----
    bindings = q(con, r"""
        SELECT file_path, original_name, binding_key, semantic_name AS name,
               origin, gate_status
        FROM semantic_binding_names
        WHERE semantic_name NOT LIKE 'renamed\_%' ESCAPE '\'
          AND accepted = 1
    """)

    # ---- clusters: accepted file/cluster names keyed by fingerprint ----
    clusters = q(con, """
        SELECT fingerprint, path, origin
        FROM island_cluster_names
        WHERE accepted = 1
    """)

    # ---- packages, channel 1: vendored modules ----
    vendored = q(con, """
        SELECT module_original_name AS module, package_name AS package,
               package_version AS version, emission_mode, status
        FROM package_attributions
    """)
    # dedup by module path: a module counts as externalized if ANY row says so
    by_mod = {}
    for r in vendored:
        m = r["module"]
        ext = r["emission_mode"] == "external_import" and r["status"] == "accepted"
        cur = by_mod.get(m)
        if cur is None:
            by_mod[m] = {"module": m, "package": r["package"],
                         "version": r["version"], "externalized": ext}
        else:
            cur["externalized"] = cur["externalized"] or ext
    vendored_clean = sorted(by_mod.values(), key=lambda x: x["module"])

    # ---- packages, channel 2: inlined island anchors ----
    island = q(con, """
        SELECT binding_name, package_name AS package, package_version AS version,
               export_specifier, function_span_start AS s, function_span_end AS e,
               tier, external_importable
        FROM package_island_anchors
    """)

    os.makedirs(out_dir, exist_ok=True)
    payload = {
        "bindings.json": bindings,
        "clusters.json": clusters,
        "packages.json": {"vendored": vendored_clean, "island_anchors": island},
    }
    for fn, data in payload.items():
        with open(os.path.join(out_dir, fn), "w") as f:
            json.dump(data, f, indent=2, ensure_ascii=False)

    human = sum(1 for b in bindings if b["origin"] == "human")
    ext_v = sum(1 for v in vendored_clean if v["externalized"])
    ext_pkgs = sorted({a["package"] for a in island
                       if a["tier"] in ("exact", "exact_alternate") and a["external_importable"]})
    print(f"gold written to {out_dir}")
    print(f"  bindings (real): {len(bindings)}  (human={human}, agent={len(bindings)-human})")
    print(f"  clusters (accepted): {len(clusters)}")
    print(f"  vendored modules: {len(vendored_clean)}  (externalized={ext_v})")
    print(f"  island anchors: {len(island)}  externalizable pkgs={len(ext_pkgs)} {ext_pkgs}")


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", required=True, help="gold (v1) project.sqlite")
    ap.add_argument("--out", default="gold")
    a = ap.parse_args()
    extract(a.db, a.out)
