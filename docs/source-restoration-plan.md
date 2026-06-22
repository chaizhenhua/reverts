# 源码还原专项 (Source-restoration project)

外化对 tree-shaken/scope-hoisted 第三方包**结构性失败**(实测:zod 214 匹配 0 外化、
cookie 120 匹配 0 外化、clsx 0 匹配)—— 它们内部互引、无干净边界,外化安全证明正确拒绝。
源码还原是**安全替代**:不把内联代码换成 `import 'pkg'`(会断裂),而是用**指纹匹配到的
真实 npm 源**让这些第三方代码**可识别、可读**,且**不改变运行行为**。

数据已在 generate 时可得:`package_source_cache`(真实源)+ `package_attributions`
(每模块的 `package_name` / `package_subpath` / `resolved_file`,含 ownership 匹配但被拒的)。

## 里程碑(按风险/价值,逐步加深)

### M1 — 识别 + 源码 sidecar(零风险,先做)
generate 写出:
- `.reverts/recognized-packages.json`:每个有 package attribution 的模块 →
  `{module_id, file_path, package, version, subpath, status}`(accepted=已外化 /
  matched=识别但未外化)。
- `.reverts/restored-sources/<pkg>@<ver>/<subpath>`:从 `package_source_cache` 落地的
  **真实可读源**,供对照。
**不动运行代码** → 100% 安全;validatable(generate 写文件,e2e 不变)。
价值:产物里**每个被识别的第三方模块都标注了真实身份**,并附带真实源。

### M2 — 包感知文件命名(低风险)
把 matched 模块的**输出文件名**改成真实包路径(`vendor/zod/classes/ZodString.ts`),
复用 `module-names` 机制 + M1 的 subpath 数据。只改文件位置,不改代码 → 安全。
价值:一眼看出哪些是 vendored zod/cookie/…。

**M2a 已实现(`vendor_path` 标注,纯 sidecar,零运行风险)** —— `recognized-packages.json`
的每个条目现带 `vendor_path` 字段:`vendor_module_path(package, subpath)` 确定性地算出
`vendor/<pkg>/<subpath>.ts`(丢 scope `@`、规范化扩展名为 `.ts`、NULL subpath → `index.ts`),
**保证**同时过 module-path 命名闸(`validate_module_path_acceptance`)与输出布局
(`is_safe_typescript_module_path`)—— 即可被 `module-names` 原样接受、不退化成
`modules/<id>-…` slug。单测覆盖映射 + 对抗输入(traversal/绝对路径/空段/怪字符)恒安全。
**这一步只标注、不动文件**(像 M1)。**M2b 待做**:把 `vendor_path` 经 `module_path_overrides`
真正落地为文件重定位 —— 机制(module-names)已被项目证明安全,但布局变更需在真实语料上
跑 e2e 验证(本环境无 DMG/DB),故留作语料内的后续。

### M3 — 模块体内联源码还原(中风险,核心)
对 matched 模块,把 minified 体**替换成真实子模块源**,并**保留导出接口**:
用 ownership 的函数级对应(minified fn ↔ real fn)推出 `真名 → minified 导出名` 别名,
发射 `…真实源…; export { RealName as minifiedName, … }`,消费者不破。
- 先做**单导出模块**(子模块只导出一个 class/fn,别名 1:1,最简单、最安全)。
- 再扩多导出(需完整别名映射)。
风险:别名映射要精确;漏一个跨模块引用就断。每次 generate 验证 ~30s(不跑慢 matcher)。
最终气闸:tsc + esbuild + e2e 全绿才接受。

### M4 — 子树边界外化(高价值,复用研究结论)
对**子树边界全是公共导出**的包(可能 react/rxjs 比 zod 干净),真正外化整子树
(见 docs/externalization-plan.md 的 subtree-boundary 设计)。需先解决 matcher 多包
匹配的内存(20 包同跑 OOM)。

## 顺序
M1(零风险、覆盖全部 matched 包)→ M2 → M3(单导出先行)→ M4。
每个里程碑 generate 即可验证(M1/M2/M3),只有 M4 需要慢/重的 matcher。

## "安全全量外化"为何结构性不可达 —— 三层独立证明(2026-06-20)

1. **逐模块证明**(`importable.rs`):匹配上的模块导出**内部绑定**(被同包其它模块消费,
   非公共 API)→ public-export-member proof 失败。实测 zod 214 匹配 0 提升、cookie 120 匹配
   0 提升、clsx 0 匹配。
2. **岛屿 barrel 合成**(`try_synthesize_plan`):tree-shaken 后没有可恢复的完整 barrel,
   只有 @opentelemetry/api(有干净 barrel)能合成;sentry/semver 等 skip。
3. **文件级边界**(本次实测):`recognized-packages.json` 里 semver 的 16 个、sentry 的 32 个
   "模块"在产物里**根本不是独立文件**(存在性 0/16)—— 它们被 scope-hoist **内联进了
   island**,与应用代码交织。没有"模块子树"可外化;子树边界外化(M4)对它们不成立。

**结论**:这些包被打包器内联进 island、互引内部绑定、没有任何干净边界。每一层安全机制
(逐模块证明、岛 barrel 合成、子树边界)都**正确拒绝**——强行外化会让读取内部绑定的消费者
断裂,违背"反编译结果能正常运行"。**安全的全量外化对 tree-shaken 内联包结构性不可达**,
不是缺机制。能安全外化的就是那 5 个有干净边界的包(@opentelemetry/api 有 barrel;
ws/node-pty/@ant 有 `require()` 外部锚点)。

**因此源码还原(M1,已实现 `71a6fc00`)是这些包的正确安全处理**:不外化(会断),而是
identify + 提供真实源,让 723 个被识别的内联第三方模块可读、可溯源,且零运行风险。

## 关键约束
- 任何里程碑都不得让产物跑不起来:M1/M2 不改代码;M3/M4 必须 tsc+esbuild+e2e 全绿才接受。
- 复用既有:M1 用 attributions + source_cache;M2 用 module-names;M3 用 ownership 函数对应;
  M4 用 importable.rs 的 public-export 别名 + planner 引用图。
