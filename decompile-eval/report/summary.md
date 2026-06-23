# Decompile SKILL eval — fresh vs gold

fresh DB: `/Users/chaizhenhua/Codes/claude-decompiled-v2/project.sqlite`

## Join coverage (health — fix first if low)

- bindings covered: **100.0%** (10352/10352)
- clusters covered: **100.0%** (1491)

## Naming

- binding coverage (fresh named / gold named): **100.0%**
- binding accuracy strict (exact): **100.0%**  | lenient (exact+fuzzy): **100.0%**
  - exact=10352 fuzzy=0 miss(on covered)=0 uncovered=0
  - fresh produced 10352 real names total
- cluster accuracy lenient: **100.0%** (exact=1491 fuzzy=0)

## Packages

- vendored precision **65.4%** / recall **44.0%** (gold 361 mods, fresh 243)
  - externalized: gold 8 vs fresh 22
- island precision **100.0%** / recall **97.2%** (gold 540 anchors, fresh 525)
