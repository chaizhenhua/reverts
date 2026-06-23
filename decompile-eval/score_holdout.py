#!/usr/bin/env python3
"""Score held-out naming: proposed (agent) TSVs vs gold TSVs, per original_name."""
import glob, json, os, re, sys

_CAMEL = re.compile(r"[A-Z]?[a-z]+|[A-Z]+(?![a-z])|\d+")
NOISE = {"the", "a", "fn", "func", "tmp", "var", "is", "to", "of", "and", "for"}
# lever 3: pure-convention synonyms shouldn't count as misses
SYN = {"regex": "pattern", "re": "pattern", "regexp": "pattern",
       "millisecond": "ms", "milliseconds": "ms", "msec": "ms",
       "directory": "dir", "configuration": "config", "cfg": "config",
       "identifier": "id", "number": "num", "maximum": "max", "minimum": "min"}


def canon(t):
    return SYN.get(t, t)


def tokens(name):
    name = re.sub(r"[^A-Za-z0-9]+", " ", name)
    out = []
    for w in name.split():
        out.extend(canon(m.group(0).lower()) for m in _CAMEL.finditer(w))
    return {t for t in out if t not in NOISE}


def match(a, b):
    if not a or not b:
        return "miss"
    if re.sub(r"[^a-z0-9]", "", a.lower()) == re.sub(r"[^a-z0-9]", "", b.lower()):
        return "exact"
    ta, tb = tokens(a), tokens(b)
    if ta and tb and len(ta & tb) / len(ta | tb) >= 0.5:
        return "fuzzy"
    return "miss"


def load(p):
    d = {}
    for line in open(p):
        parts = line.rstrip("\n").split("\t")
        if len(parts) >= 2 and parts[0]:
            d[parts[0]] = parts[1]
    return d


base = os.path.dirname(os.path.abspath(__file__))
gold_dir = os.path.join(base, "holdout/gold")
prop_dir = os.path.join(base, "holdout/proposed")

tot = {"gold": 0, "named": 0, "exact": 0, "fuzzy": 0, "miss": 0}
misses = []
print(f"{'file':45} {'gold':>5} {'named':>5} {'exact':>5} {'fuzzy':>5} {'miss':>5} {'cov':>6} {'acc':>6}")
for gp in sorted(glob.glob(os.path.join(gold_dir, "*.tsv"))):
    name = os.path.basename(gp)[:-4]
    # find matching proposed file (basename may differ slightly); match by stem prefix
    cands = glob.glob(os.path.join(prop_dir, "*.tsv"))
    pp = None
    key = name.split("__")[-1][:12]
    for c in cands:
        if key in os.path.basename(c) or os.path.basename(c)[:-4].split("__")[-1][:12] in name:
            pp = c; break
    gold = load(gp)
    prop = load(pp) if pp else {}
    e = f = m = 0
    for orig, gname in gold.items():
        pn = prop.get(orig)
        if pn is None:
            continue
        r = match(gname, pn)
        if r == "exact": e += 1
        elif r == "fuzzy": f += 1
        else:
            m += 1
            if True:
                misses.append({"file": name, "orig": orig, "gold": gname, "proposed": pn})
    named = e + f + m
    g = len(gold)
    cov = named / g if g else 0
    acc = (e + f) / named if named else 0
    print(f"{name[:45]:45} {g:>5} {named:>5} {e:>5} {f:>5} {m:>5} {cov:>6.1%} {acc:>6.1%}")
    for k, v in (("gold", g), ("named", named), ("exact", e), ("fuzzy", f), ("miss", m)):
        tot[k] += v

g = tot["gold"]; named = tot["named"]
print("-" * 100)
print(f"{'TOTAL':45} {g:>5} {named:>5} {tot['exact']:>5} {tot['fuzzy']:>5} {tot['miss']:>5} "
      f"{named/g:>6.1%} {(tot['exact']+tot['fuzzy'])/named:>6.1%}")
print(f"\ncoverage (named/gold): {named/g:.1%}")
print(f"accuracy strict (exact/named): {tot['exact']/named:.1%}")
print(f"accuracy lenient (exact+fuzzy/named): {(tot['exact']+tot['fuzzy'])/named:.1%}")
print(f"end-to-end useful (exact+fuzzy/gold): {(tot['exact']+tot['fuzzy'])/g:.1%}")
json.dump(misses, open(os.path.join(base, "holdout/misses.json"), "w"), indent=2, ensure_ascii=False)
print(f"\n{len(misses)} sample misses -> holdout/misses.json (candidates for LLM judge: many may be semantically correct but vocab-different)")
