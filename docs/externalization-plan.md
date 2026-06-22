# 充分外化工程方案 (Externalization project plan)

目标:**外化能外化的内联第三方库**。每外化一个库,其全部文件从一方树中消失 →
直接减少命名工作量,并缩小需要审计/维护的一方代码面。这是比"继续命名长尾"更高杠杆的方向。

---

## 0. 现状基线(已实测)

- 生成树 **1702** 个 `.ts`:~80 vendored 模块语义化;**121 island cluster 已命名**;
  约 **1531** 未命名。
- `package_source_cache` 缓存 **~70 个包**(zod / react / react-dom / rxjs / fflate /
  tar / winston / jsonwebtoken / semver / @sentry/electron …)。
- **实际已外化:5 个** —— `@opentelemetry/api`(island barrel 合成)+
  `ws` / `node-pty` / `@ant/claude-native` / `@ant/claude-swift`(module-path 外化)。
- `package_attributions`:**13 accepted / 710 rejected**。
- 大量内联第三方库(确认在产物中):zod 47 文件、ajv 39、semver 12、winston 9、
  @sentry/electron 6、react 2,以及 agent 命名出的 axios / undici / rxjs / fflate /
  fastify / tar / intl-messageformat / MCP SDK 等。

诊断工具:`REVERTS_DEBUG_ISLAND_PKG=1 generate`(commit `14e48a52`)逐包打印
为何外化失败。

## 外化机制三步(回顾)

1. **锚定 anchor**:把内联岛代码匹配到真实包 `(name, version)`。确定性指纹匹配
   (`match-packages`),或对无 node_modules 身份的库由 agent 提议
   (`island-package-candidates`)+ 指纹确认。
2. **源缓存 cache**:真实包源进 `package_source_cache`(已有 ~70)。
3. **barrel 合成 synthesize**:解析真实 `index` → `子模块路径 → 导出名` 映射 →
   每个内联单元换成 `import * as ns from 'pkg'; const X = ns.Name;` + init shim,删内联。

---

## 四期分解(按 ROI / 风险)

### Phase 1 — index-shape 解析扩展(低风险,前置能力)
- **现状**:`reverts_js::parse_index_reexports` 只读 CJS `module.exports = { … }`
  (含 `const X = require('./x'); module.exports = { X }` 与 member-pick)。
- **缺**:ESM `export { … }`、`export { X } from './x'`、tslib `__exportStar(require('./x'), exports)`、
  `Object.defineProperty(exports, 'X', { get: … })`、`tslib.__exportStar`。
- **改**:给解析器加这些形态(纯 `reverts-js`,可单测)。
- **影响**:`@sentry/electron`(ESM `export {}`)等 ESM 形态包的 index 能被解析。
- **风险**:低(纯解析,单测覆盖)。**单独收编文件 ≈ 0**(是 Phase 3/4 的前置)。

### Phase 2 — semver 式过度归因修复(中风险)
- **现状**:`semver` index 已加载,但 **32 个 trivial 单元全部被归到
  `semver/classes/range`** → over-match gate(`fb4aa412`)正确拒绝整包。
- **根因**:指纹匹配器把 32 个小模块误匹配到 range.js(大、独特,易被碰撞)。
- **两条改法**:
  - **A 精度(推荐)**:锚定阶段加阈值 —— 单元绑定数 / 指纹分必须与真实子模块相称,
    否则不归因。剔除 32 个假阳性 → 真 range 唯一 → semver 干净外化,**gate 保持严格**。
  - **B 合成层韧性**:某子模块被 N>1 单元 claim 时,不整包 bail,而是排除该歧义子模块的
    单元(留内联),外化其余 1:1 干净映射的成员。部分外化。
- **风险**:中。gate 是为修 RxJS `Class extends value undefined` 加的 —— 改动后**必须回归
  验证 RxJS 不再坏**(已有 over_matched_duplicate_submodule_is_not_synthesized 测试)。
- **预期收编**:semver 12 文件 + 同类 over-match 的包。
- **依赖**:归因在 DB 里 → 走精度路线 A 需 **re-match**(见 §re-match);路线 B 可纯合成层。

### Phase 3 — 锚定覆盖缺口(最高 ROI,高投入)← 最大收编
- **现状**:zod(47 文件)/ react / rxjs / fflate / tar / winston 等**已缓存源但 0
  attribution** —— matcher 从没把内联代码连到它们,连 island-package 候选都不是。
- **先调查根因**(低风险):为何没归因?可能
  (a) 当时 match-packages 没覆盖到;
  (b) tree-shaken 后内联单元指纹与真实包不匹配;
  (c) 这些库以 ESM / 无 `__commonJS` wrapper 形态内联,锚定机制(认 CJS 单元三元组)不识别。
- **改**:据根因提升锚定覆盖(很可能是 (c) —— 锚定器只认 CJS-wrapper 单元,认不出
  scope-hoist 的 ESM 内联;需扩锚定到 ESM 内联形态)。
- **风险**:高 —— 需 fresh re-import + match-packages(在已匹配 DB 上重跑会
  `BundleDetectorAmbiguous`,**必须 fresh import**)。
- **预期收编**:zod 47 + ajv 39 + react / rxjs / winston / … → **可能 100–300+ 文件从树消失**。

### Phase 4 — 跨包 barrel(sentry,中风险,新机制)
- **现状**:`@sentry/electron` 的 index 从 **sibling 包**(`@sentry/node` / `@sentry/core`)
  re-export,不是对自身子模块的映射;内联的实际是 @sentry/node/core 的实现。
- **改**:barrel 合成支持跨包 —— index 的 `export { X } from '@sentry/node'` 把内联单元映到
  `@sentry/node` 的命名空间成员(或保留为 `@sentry/electron` 的 re-export)。**需 Phase 1**。
- **风险**:中(新映射模型)。
- **预期收编**:sentry 6 文件 + sentry 生态(metrics / tracing / http-instrumentation …)。

---

## re-match 安全流程(Phase 2-A / Phase 3 需要)

绝不在 `project.sqlite` 上重跑 `match-packages`(会重复 synthetic sources →
`BundleDetectorAmbiguous`)。**Fresh** 流程(见 memory `chain-split-dag-routing`):

1. `hdiutil attach DMG` → `npx @electron/asar extract … /private/tmp/claude-app` +
   `cp -R app.asar.unpacked/*`。
2. `/tmp/gen_import_evidence.py` → `import-unpacked` → `module-classify --auto --apply`
   → `match-packages --package-source-root … --apply`(**含本期的锚定精度/覆盖改进**)。
3. `generate --source-root src`。

**命名迁移**:`cluster-names` 是 **fingerprint-keyed**,内容不变则 fingerprint 不变 →
已命名的 121 个 cluster 在 fresh DB 上**仍适用**;只需把 `island_cluster_names` 行从旧 DB
迁到 fresh DB(`SELECT … ` + `cluster-names --batch`)。同理 `module_path_overrides` /
`semantic_binding_names` / `symbols.semantic_name`。

---

## 预期总收编

Phase 2+3+4 成功后:zod 47 + ajv 39 + semver 12 + winston 9 + sentry 6 + react 2 +
axios / undici / rxjs / fflate / tar / … → **粗估 150–300+ 文件从一方树消失**,命名长尾从
~1531 降到 1000 出头,一方代码面显著缩小,且这些库的运行时换成 `import 'pkg'`(更忠实于
"这是第三方依赖"的事实)。

## 建议投入顺序

1. **Phase 1**(低风险前置)先做 —— 解锁 ESM index 解析。
2. **Phase 3 仅调查**(低风险)—— fresh re-match + 诊断,定位 zod/react 为何 0 attribution。
   这一步决定 Phase 3 的真实 ROI,且不改任何已发布代码。
3. 据调查结果实施 Phase 2(精度)/ 3(覆盖)/ 4(跨包)。

## 实施中发现的关键阻塞(2026-06-19,实测)

**P1 已实现并提交**(`a22cff80`):index 解析器现支持 ESM `export { X } from './sub'`
(member-pick),单测覆盖。这是 ESM 形态包外化的前置能力 —— 但对当前语料**无即时收编**,
因为已锚定的包(@sentry/electron 等)用的是**跨包** `export {X}`(从 sibling 包,非本地子模块),
P1 的本地子模块 re-export 形态用不上;真正用到 P1 的包需要先被锚定。

**P2/P3/P4 共同的硬阻塞 = 重新锚定(re-anchoring)计算上不可行。**
根因确认:只有 **12 个包**曾被提议为 island-package-candidate(OTel/Sentry 栈 + semver);
zod/react/rxjs/ajv 等**从未被提议** → 从未锚定(源虽缓存)。补外化必须重新锚定。
实测:`island-package-candidates --accept zod` + `match-packages --package-name zod --apply`
在大 island 上跑了 **55+ 分钟仍未收敛**(被 kill,无任何写入)。即:
- 增量 per-package 重匹配 ≈ 1 小时/包且可能不收敛 → 外化几十个包**不可行**。
- 原始 12 个候选是**一次性**匹配出来的(2026-06-13 建库时),不是增量加的。

**结论:外化工程的真正关键路径 = 先做 matcher 锚定的性能优化**(island 指纹匹配疑似
O(island_units × pkg_functions) 的高代价,且 `--package-name` 未能有效限制 island 扫描),
否则 P2/P3/P4 都卡在这一步。可选的非交互路径:把所有目标候选(zod/react/rxjs/ajv/…)
一次性提议,跑一次**整夜批量** `match-packages`,再外化 —— 但单次可能数小时。

**当前状态干净**:实验全在 `project.sqlite` 的副本上做,真实 DB 与 app(121 命名 cluster、
e2e PASS)未受影响。已提交:P1 解析器、诊断工具(`14e48a52`)、本方案文档。

## 性能阻塞修复进展(2026-06-19,实施中)

发现并修复**两个** `--package-name` 过滤丢失导致的性能阻塞:
1. **cascade pass**(`33e4cfba`):`CascadeMatchPass` 没传 `package_filter` → 对全部缓存包
   做指纹级联。修复后 cascade_match **55min(不收敛)→ 11.6s**,整 pipeline 28s。
2. **island anchoring**(`compute_island_anchors` 的 `island_corpus`):同样没过滤 →
   把全部 540 个 no-module 库源对每个 island CJS 单元匹配 → post-pipeline 40+ min。
   修复:按 `package_filter` 把 island_corpus 裁到请求的包。约降到 ~corpus 比例
   (540→单包约 50,即 ~4min/包)。

**仍存在的根本低效**:island anchoring 是 O(island_units × corpus),且每次 match 都重算
全部 island 单元指纹。增量 per-package ≈ 4min/包;**全量目标包应一次性提议、跑一次 match**
(corpus = 所有候选包,~20-30min 一次),比 N 次增量更省。后续可加 island-unit 指纹缓存。

## 子集外化研究结论(2026-06-19,定论)

**子集外化机制已存在且在工作** —— `ownership/importable.rs::promote_anonymous_bundle_external_imports`:
对 source-only 的匹配,当**模块自身导出的成员 ⊆ 包的公共导出**(public-export-member
proof)时,提升为 external_importable。planner 侧还有"external-package adapter"兜底:
任何**被消费的绑定**若无法从包公共面重新提供,就保持内联 —— 所以 build 永不破。

**为什么 21 个候选包(含 zod)过不了 —— 结构性原因,非缺机制:**
zod 端到端实测(并行后 51min):214 个 ownership 匹配(确实匹配上 zod),但**全部
rejected**,reason="matched package ownership, but the evidence does not prove a safe
single external import"。`promote_anonymous_bundle_external_imports` 提升了 **0 个** zod 模块
—— 说明 zod 的内联模块**导出了内部绑定**(不在 zod 公共 API 里)。esbuild 把 zod
tree-shake + scope-hoist 后,模块间**交叉引用内部绑定**;把它们换成 `import {…} from "zod"`
会让读取这些内部绑定的消费者全部断裂。`external_importable = external_specifier.is_some()`
(`exact_hint.rs:110`)—— zod 全程 scope-hoist、bundle 里没有 `require("zod")` 边界,
所以连一个外部导入锚点都没有。

**定论:21 个候选包无法(即使部分)外化**,不是因为缺机制(子集外化已实现),而是
它们被打包器内联成**交叉引用内部绑定、无干净公共边界**的形态。安全证明正确拒绝。
能外化的 5 个包正是有干净边界的(@opentelemetry/api 有可重建的 barrel;ws/node-pty/@ant
有 `require()` 外部导入锚点)。**这是外化 tree-shaken scope-hoisted 代码的根本上界,
不是可修的 bug。**

**本轮交付的真实成果(可复用):** matcher 三个性能修复(cascade 过滤 55min→12s `33e4cfba`、
island corpus 过滤 `e6c2e34d`、cascade per-subject 并行 8x `9677ba11`)——让 matcher 在能外化的
包上可用;诊断工具 `14e48a52`。这些独立于外化能否扩展,都是净改进。

## 找到并验证的额外外化:semver(第 6 个,2026-06-20)

回答"还有没有优化空间":**有**。semver 不是结构性不可外化,而是被一个**可修的精度 bug**
挡住 —— 它的 island barrel 合成在 generate 时跑(~30s 验证,不用慢 matcher),但 over-match
gate(`fb4aa412`)因 `classes/range` 被 44 个 trivial 单元误归(指纹碰撞)而**整包 bail**。

**修复**(`c6ccfeec`):让合成对 over-match **有韧性** —— 只**排除**被超额认领的子模块的单元
(留内联、不重绑,避免 RxJS blank),合成其余 1:1 干净成员。实测:
`excluded 44 over-matched unit(s), synthesized 3 clean member(s)` →
`import * as _pkg_semver from 'semver'`。**esbuild 干净、e2e PASS 523⊇310**。
**安全外化从 5 → 6 个包。**

**剩余 island synth 候选(本语料):**
- `@sentry/electron`:`index.js` 只 throw 守卫(真入口是 `main/index.js`),且 `main` barrel
  从 sibling 包(@sentry/node 等)**跨包 re-export** → 需主入口解析 + 跨包映射,实质工程(P4)。
- `node-pty`/`ws`:native,已走 module-path 外化;island 部分无 JS barrel。

所以这一轮**安全外化的额外收获是 semver**;再多(sentry)需要跨包 barrel 这个大件。
resilient 修复本身是**通用机制改进**(任何 over-attributed 包都受益),不止 semver。

**sentry 深入调查(2026-06-20,确认不可快修):** @sentry/electron 的 32 个归因**全是
NULL subpath** —— island 单元在**包**粒度匹配上,但 matcher 没钉到具体子模块文件,所以
barrel 合成(单元→子模块→导出名)**无可映射**。且真 barrel(`main/index.js`)混用本地
子模块 `require('./integrations/…')` 与**跨包** `require('@sentry/node')`。需要两件大事:
(1) matcher 精度(给每个单元钉子模块),(2) 跨包 barrel 映射。都不是快修。

**本语料安全外化的现实上界 = 6 个包**(@opentelemetry/api、semver、ws、node-pty、@ant×2)。
node-pty/ws 已走 module-path;@opentelemetry/core 等其余候选 0 anchored island 单元、进不了
synth 路径。20 个 tree-shaken 包结构性不可外化(三层证明)。

## sentry 外化达成 —— 7/7(2026-06-20,`0cc687f9`,SUPERSEDES「6 个上界」)

上面「sentry 不可快修 / NULL subpath / 上界 6」的结论**部分错判**:NULL subpath 是
`package_attributions` 表的,但 island 合成走的是 **`package_island_anchors`**,该表里 sentry 的
33 个 anchor **都有** submodule `export_specifier`(5 个子模块,11 个 recognized 单元)。真正阻塞是
**`index=MISSING`**:loader 只读根 `index.js`,而 sentry 的根 index 是 dual-entry **guard**
(只 `throw "use /main or /renderer"`,re-export 为空)→ 合成器拿到空 index 直接 skip。

**修复(四层,各有单测):**
1. **解析器**:支持 `exports.Name = sub.Member` / `exports.Name = sub` 的逐成员赋值 barrel
   形态(sentry `main/index.js`)。跨包 `node.X`(`require('@sentry/node')`)行 naming 无 `./`
   子模块 → 自动忽略;**所以并不需要真正的跨包机制**,被锚的单元全是本地子模块。
2. **`PackageIndexReexports::rebased/merge`**:把 subpath barrel 的 relpath 提升为包根相对
   (`transports/x`→`main/transports/x`)并打上公共 import specifier(`@sentry/electron/main`)。
3. **CLI loader**:根 index 为空(guard)时,回退到公共 subpath barrel(`<dir>/index.js`,dir 由
   锚定子模块顶层目录推出,排除 esm/cjs/dist 等构建目录)。**仅在根空时触发**,故 6 个有可用
   根 index 的包零影响。
4. **model + planner 发射**:`SynthesizedMember` 带可选 per-member `import_specifier`;合成 barrel
   按 specifier 分组,每个 subpath 发一条 `import * as <alias>`(绝不 import 不可用的 guard 裸包)。

**实测**:`excluded 8 over-matched, synthesized 2 clean member(s)` →
`import '@sentry/electron/main'` + `/renderer`,dep 注册 7.13.0。**7/7 已识别包全部外化**。
验证:esbuild bundle 干净(无 dangling export)、e2e 等价 **PASS(recovered 2956 ⊇ reference 310,
loadError=null)**。2 个 clean 成员之外的 9 个 over-matched 单元按 resilient gate 留内联(安全)。

## 风险提示

- gate 放松 / 锚定放宽都可能重新产出**坏的外化**(把真实模块 body 清空)→ 每期必须
  esbuild + e2e + DanglingNamedImport 审计全绿,且回归 RxJS/已外化包不退化。
- Phase 3 依赖 fresh 环境重建(fragile);命名/语义数据需按上面的 fingerprint/id-key 迁移。
