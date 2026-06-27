# Island-cluster tail naming (Claude app)

`island-cluster-tail-143.tsv` is the `cluster-names --batch` worklist that named the
last **143 unnamed island clusters** of the Claude desktop decompile, driving the
`UnnamedMechanicalPath` audit (and the Phase-4 cluster gate) to **zero**.

Format: `accept <TAB> <fingerprint> <TAB> <relative-path> <TAB> <evidence>` — paths
are relative to `modules/island/`, keyed by the cluster's stable content fingerprint
(survives regenerate; the `cluster-<n>` number does not).

## How it was produced
Generated the project, read `.reverts/island-clusters.json`, filtered to the 143
entries whose `path` was still `cluster-<n>.ts`, then fan-out classified them (7
agents reading the cluster bodies, no access to the gold names):

- **136 inlined third-party** → `vendor/<pkg>` by literal evidence (package strings,
  `super('@scope/pkg', VERSION)`, exported class, `./vendor/<pkg>` sibling imports):
  opentelemetry (instrumentations + semantic-conventions), rxjs, ajv, fs-extra,
  winston, pako, xmldom, readable-stream, https-proxy-agent, agent-base, yauzl,
  ssh2, plist, jszip, setimmediate, …
- **6 first-party** (the large eager-residual chunks `cluster-285x/287x`):
  `auth/oauth`, `auth/sso`, `auth/oidc`, `agent/tool-permissions`,
  `agent/session-error-classification`, `plugins/cli-manifest-validation`.
- **1 generic util** with no package evidence → `util/stringify-values`.

## Apply / reproduce
```bash
reverts-cli cluster-names --input <project.sqlite> --project-id 1 \
  --batch island-cluster-tail-143.tsv --origin agent --apply
reverts-cli generate --input <project.sqlite> --project-id 1 --output <out> --source-root src
# verify: 0 cluster-<n>.ts emitted, UnnamedMechanicalPath audit clean
```

Applied to `claude-decompiled-v2/project.sqlite` (backup:
`project.sqlite.bak-pre-cluster-tail`). Result: 0 cluster-N, generate exit 0, audit
fully clean (parse / relative-import-targets / dangling-named-import all pass).
