#!/usr/bin/env python3
"""Score a FRESH decompile DB (current SKILL run) against the gold answer-key.

Outputs:
  report/summary.md   - human-readable metrics with deltas vs gold
  report/grading.json - skill-creator assertion format (text/passed/evidence)

Metrics:
  Join coverage (health) - if low, fingerprinting/partition is unstable; trust nothing else.
  Package precision/recall - vendored (by node_modules path) + island (by binding+span).
  Naming coverage          - fresh named / gold named (relative to gold standard).
  Naming accuracy          - of fresh names on gold-named bindings: exact / fuzzy / miss.

Note: gold names ARE semantic (not original identifiers), so "accuracy" compares
fresh-semantic vs gold-semantic by normalized token overlap. The fuzzy bucket is
where an LLM judge should adjudicate (incl. "fresh better than gold").
"""
import argparse, json, os, re, sqlite3, sys


def q(con, sql, args=()):
    cur = con.execute(sql, args)
    cols = [c[0] for c in cur.description]
    return [dict(zip(cols, r)) for r in cur.fetchall()]


def load(gold_dir, fn):
    with open(os.path.join(gold_dir, fn)) as f:
        return json.load(f)


_CAMEL = re.compile(r"[A-Z]?[a-z]+|[A-Z]+(?![a-z])|\d+")


def tokens(name):
    # strip a leading mechanical "renamed_" if any slipped through, then tokenize
    name = re.sub(r"[^A-Za-z0-9]+", " ", name)
    out = []
    for w in name.split():
        out.extend(m.group(0).lower() for m in _CAMEL.finditer(w))
    # drop noise tokens that carry no semantic signal
    return {t for t in out if t not in {"the", "a", "fn", "func", "tmp", "var"}}


def name_match(a, b):
    if a is None or b is None:
        return "miss"
    na, nb = re.sub(r"[^a-z0-9]", "", a.lower()), re.sub(r"[^a-z0-9]", "", b.lower())
    if na == nb:
        return "exact"
    ta, tb = tokens(a), tokens(b)
    if ta and tb:
        j = len(ta & tb) / len(ta | tb)
        if j >= 0.5:
            return "fuzzy"
    return "miss"


def fresh_bindings(con):
    rows = q(con, r"""
        SELECT file_path, original_name, binding_key, semantic_name AS name
        FROM semantic_binding_names
        WHERE semantic_name NOT LIKE 'renamed\_%' ESCAPE '\' AND accepted = 1
    """)
    strict = {(r["file_path"], r["original_name"], r["binding_key"]): r["name"] for r in rows}
    lenient = {(r["original_name"], r["binding_key"]): r["name"] for r in rows}
    return strict, lenient, len(rows)


def score_bindings(gold, con):
    strict, lenient, fresh_total = fresh_bindings(con)
    exact = fuzzy = miss = covered = 0
    fuzzy_samples = []
    for g in gold:
        k3 = (g["file_path"], g["original_name"], g["binding_key"])
        k2 = (g["original_name"], g["binding_key"])
        fname = strict.get(k3) or lenient.get(k2)
        if fname is None:
            miss += 1
            continue
        covered += 1
        m = name_match(g["name"], fname)
        if m == "exact":
            exact += 1
        elif m == "fuzzy":
            fuzzy += 1
            if len(fuzzy_samples) < 40:
                fuzzy_samples.append({"original": g["original_name"], "gold": g["name"], "fresh": fname})
        else:
            miss += 1
    n = len(gold)
    return {
        "gold_total": n, "fresh_total": fresh_total,
        "coverage": covered / n if n else 0.0,
        "exact": exact, "fuzzy": fuzzy, "miss_on_covered": miss - (n - covered),
        "uncovered": n - covered,
        "accuracy_strict": exact / covered if covered else 0.0,          # exact only
        "accuracy_lenient": (exact + fuzzy) / covered if covered else 0.0,  # exact+fuzzy
        "fuzzy_samples": fuzzy_samples,
    }


def score_clusters(gold, con):
    fresh = {r["fingerprint"]: r["path"] for r in
             q(con, "SELECT fingerprint, path FROM island_cluster_names WHERE accepted=1")}
    covered = exact = fuzzy = 0
    for g in gold:
        fp = g["fingerprint"]
        if fp not in fresh:
            continue
        covered += 1
        m = name_match(os.path.basename(g["path"]), os.path.basename(fresh[fp]))
        if m == "exact":
            exact += 1
        elif m == "fuzzy":
            fuzzy += 1
    n = len(gold)
    return {"gold_total": n, "coverage": covered / n if n else 0.0,
            "exact": exact, "fuzzy": fuzzy,
            "accuracy_lenient": (exact + fuzzy) / covered if covered else 0.0}


def score_packages(gold, con):
    # vendored: join by node_modules path
    g_vend = {v["module"]: v for v in gold["vendored"]}
    f_rows = q(con, """SELECT module_original_name AS module, package_name AS package,
                              emission_mode, status FROM package_attributions""")
    f_vend = {}
    for r in f_rows:
        ext = r["emission_mode"] == "external_import" and r["status"] == "accepted"
        cur = f_vend.get(r["module"])
        if cur is None:
            f_vend[r["module"]] = {"package": r["package"], "externalized": ext}
        else:
            cur["externalized"] = cur["externalized"] or ext

    # detection P/R: did fresh attribute the module to the same package?
    tp = sum(1 for m, g in g_vend.items()
             if m in f_vend and f_vend[m]["package"] == g["package"])
    detected = len(f_vend)
    vend = {
        "gold_modules": len(g_vend), "fresh_modules": detected,
        "precision": tp / detected if detected else 0.0,
        "recall": tp / len(g_vend) if g_vend else 0.0,
        "gold_externalized": sum(1 for v in g_vend.values() if v["externalized"]),
        "fresh_externalized": sum(1 for v in f_vend.values() if v["externalized"]),
    }

    # island anchors: join by (binding_name, span)
    g_isl = {(a["binding_name"], a["s"], a["e"]): a for a in gold["island_anchors"]}
    f_isl = {(r["binding_name"], r["s"], r["e"]): r for r in q(con,
             """SELECT binding_name, package_name AS package,
                       function_span_start AS s, function_span_end AS e, tier
                FROM package_island_anchors""")}
    itp = sum(1 for k, g in g_isl.items()
              if k in f_isl and f_isl[k].get("package") == g["package"])
    isl = {
        "gold_anchors": len(g_isl), "fresh_anchors": len(f_isl),
        "precision": itp / len(f_isl) if f_isl else 0.0,
        "recall": itp / len(g_isl) if g_isl else 0.0,
    }
    return {"vendored": vend, "island": isl}


def pct(x):
    return f"{100*x:.1f}%"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gold", default="gold")
    ap.add_argument("--fresh", required=True, help="fresh run project.sqlite")
    ap.add_argument("--out", default="report")
    a = ap.parse_args()

    con = sqlite3.connect(a.fresh)
    b = score_bindings(load(a.gold, "bindings.json"), con)
    c = score_clusters(load(a.gold, "clusters.json"), con)
    p = score_packages(load(a.gold, "packages.json"), con)

    os.makedirs(a.out, exist_ok=True)
    md = []
    md.append(f"# Decompile SKILL eval — fresh vs gold\n")
    md.append(f"fresh DB: `{a.fresh}`\n")
    md.append("## Join coverage (health — fix first if low)\n")
    md.append(f"- bindings covered: **{pct(b['coverage'])}** ({b['gold_total']-b['uncovered']}/{b['gold_total']})")
    md.append(f"- clusters covered: **{pct(c['coverage'])}** ({c['gold_total']})\n")
    md.append("## Naming\n")
    md.append(f"- binding coverage (fresh named / gold named): **{pct(b['coverage'])}**")
    md.append(f"- binding accuracy strict (exact): **{pct(b['accuracy_strict'])}**  | lenient (exact+fuzzy): **{pct(b['accuracy_lenient'])}**")
    md.append(f"  - exact={b['exact']} fuzzy={b['fuzzy']} miss(on covered)={b['miss_on_covered']} uncovered={b['uncovered']}")
    md.append(f"  - fresh produced {b['fresh_total']} real names total")
    md.append(f"- cluster accuracy lenient: **{pct(c['accuracy_lenient'])}** (exact={c['exact']} fuzzy={c['fuzzy']})\n")
    md.append("## Packages\n")
    v, i = p["vendored"], p["island"]
    md.append(f"- vendored precision **{pct(v['precision'])}** / recall **{pct(v['recall'])}** "
              f"(gold {v['gold_modules']} mods, fresh {v['fresh_modules']})")
    md.append(f"  - externalized: gold {v['gold_externalized']} vs fresh {v['fresh_externalized']}")
    md.append(f"- island precision **{pct(i['precision'])}** / recall **{pct(i['recall'])}** "
              f"(gold {i['gold_anchors']} anchors, fresh {i['fresh_anchors']})\n")
    with open(os.path.join(a.out, "summary.md"), "w") as f:
        f.write("\n".join(md))

    # skill-creator grading.json — assertions with thresholds
    def assertion(text, passed, evidence):
        return {"text": text, "passed": bool(passed), "evidence": evidence}
    grading = {"expectations": [
        assertion("Join coverage >= 90% (fingerprint/partition stable)", b["coverage"] >= 0.90,
                  f"binding coverage {pct(b['coverage'])}, cluster {pct(c['coverage'])}"),
        assertion("Binding naming coverage >= gold-relative 80%", b["coverage"] >= 0.80,
                  f"{pct(b['coverage'])}"),
        assertion("Binding naming accuracy (lenient) >= 70%", b["accuracy_lenient"] >= 0.70,
                  f"exact {pct(b['accuracy_strict'])}, lenient {pct(b['accuracy_lenient'])}"),
        assertion("Vendored package precision >= 95% (no false attributions)", v["precision"] >= 0.95,
                  f"precision {pct(v['precision'])}, recall {pct(v['recall'])}"),
        assertion("Vendored package recall >= 90%", v["recall"] >= 0.90, f"recall {pct(v['recall'])}"),
        assertion("Island anchor precision >= 95%", i["precision"] >= 0.95,
                  f"precision {pct(i['precision'])}, recall {pct(i['recall'])}"),
        assertion("Externalized package count >= gold", v["fresh_externalized"] >= v["gold_externalized"],
                  f"fresh {v['fresh_externalized']} vs gold {v['gold_externalized']}"),
    ]}
    with open(os.path.join(a.out, "grading.json"), "w") as f:
        json.dump(grading, f, indent=2, ensure_ascii=False)
    with open(os.path.join(a.out, "fuzzy_samples.json"), "w") as f:
        json.dump(b["fuzzy_samples"], f, indent=2, ensure_ascii=False)

    print("\n".join(md))
    npass = sum(1 for e in grading["expectations"] if e["passed"])
    print(f"\n=> {npass}/{len(grading['expectations'])} assertions passed. "
          f"see {a.out}/summary.md, grading.json, fuzzy_samples.json")


if __name__ == "__main__":
    main()
