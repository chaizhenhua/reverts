#!/usr/bin/env python3
import glob, json, os, re, sys
from score_holdout import match, load  # reuse normalizer (incl. lever-3 synonyms)

PROP = sys.argv[1] if len(sys.argv) > 1 else "proposed"
base = os.path.dirname(os.path.abspath(__file__))
H = os.path.join(base, "holdout2")
train = {l.strip().split("/")[-1].replace(".ts", "") for l in open(os.path.join(base, "holdout2_train.txt")) if l.strip()}

def stem(fp):  # gold/proposed basename -> last segment
    return os.path.basename(fp)[:-4].split("__")[-1]

rows = []
agg = {"train": {}, "test": {}}
misses = []
for gp in sorted(glob.glob(os.path.join(H, "gold/*.tsv"))):
    nm = os.path.basename(gp)[:-4]
    seg = nm.split("__")[-1]
    split = "train" if seg in train else "test"
    pp = os.path.join(H, PROP, os.path.basename(gp))
    gold = load(gp); prop = load(pp) if os.path.exists(pp) else {}
    e=f=m=0
    for orig, gname in gold.items():
        pn = prop.get(orig)
        if pn is None: continue
        r = match(gname, pn)
        if r=="exact": e+=1
        elif r=="fuzzy": f+=1
        else:
            m+=1; misses.append({"orig":orig,"gold":gname,"proposed":pn,"file":seg})
    named=e+f+m; g=len(gold)
    rows.append((split,seg,g,named,e,f,m))
    d=agg[split]
    for k,v in (("gold",g),("named",named),("exact",e),("fuzzy",f),("miss",m)): d[k]=d.get(k,0)+v

print(f"{'split':6}{'file':40}{'gold':>5}{'named':>6}{'exact':>6}{'fuzzy':>6}{'miss':>5}{'cov':>7}{'good%':>7}")
for split,seg,g,named,e,f,m in rows:
    print(f"{split:6}{seg[:40]:40}{g:>5}{named:>6}{e:>6}{f:>6}{m:>5}{named/g:>7.1%}{(e+f)/named:>7.1%}")
for split in ("train","test"):
    d=agg[split]
    if not d: continue
    g,named=d["gold"],d["named"]
    print(f"--- {split.upper()}: cov {named/g:.1%}  string-good(exact+fuzzy) {(d['exact']+d['fuzzy'])/named:.1%}  string-miss {d['miss']}")
json.dump(misses, open(os.path.join(H,"misses.json"),"w"), indent=2, ensure_ascii=False)
print(f"\n{len(misses)} string-misses -> holdout2/misses.json (to judge)")
