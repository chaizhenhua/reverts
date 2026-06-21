# submit_module_decompilation Format Reference

## Parameters

### `project_id` (required)
Integer project ID. Get from `list_projects()`.

### `module` (required)
```json
{
  "name": "c42",           // Module original_name (required)
  "sem": "utils/logger",   // Semantic name — must be unique across the project
  "cat": "app"             // Category: app, pkg, std, unk
}
```

For package modules:
```json
{
  "name": "m5",
  "sem": "lodash/chunk",
  "cat": "pkg",
  "pkg": "lodash",
  "ver": "4.17.21"
}
```

### `symbols` (optional)
Map of original name to semantic spec:
```json
{
  "C3": "UserService:1",      // Rename + mark exported (:1)
  "f7": "fetchProfile",       // Rename (not exported)
  "k2": ":1"                  // Export-only, no rename
}
```

The `:1` suffix marks a symbol as exported.

### `locals` (optional)
Local variable renames scoped to a parent symbol:
```json
{
  "C3": {
    "a": "userId",
    "b": "response:Response"   // With type annotation
  }
}
```

### `types` (optional)
Type definitions to add to the module:
```json
[
  "interface User { id: string; name: string; email: string; }",
  "type UserRole = 'admin' | 'user' | 'guest' // refs: User"
]
```

The `// refs: A, B` comment tells the system which symbols reference this type.

### `global_variables` (optional)
Cross-module global variable renames:
```json
{
  "ag2": "rxjsObservable"
}
```

### `symbol_types` (optional)
Type annotations for specific symbols:
```json
{
  "C3": {
    "return_type": "Promise<User>",
    "parameters": [
      { "name": "id", "type": "string" },
      { "name": "options", "type": "RequestOptions", "optional": true }
    ]
  }
}
```

### `update_mode` (optional)
- `"merge"` (default): Only updates non-empty fields
- `"override"`: Completely replaces existing data
- `"propagate"`: Applies type propagation rules

## Examples

### Minimal — module name only
```json
{
  "project_id": 49,
  "module": { "name": "c42", "sem": "utils/logger", "cat": "app" }
}
```

### App module with symbols
```json
{
  "project_id": 49,
  "module": { "name": "rEB", "sem": "api/user-service", "cat": "app" },
  "symbols": {
    "rEB": "UserService:1",
    "f7": "fetchProfile",
    "v2": "API_BASE_URL"
  },
  "locals": {
    "f7": { "a": "userId", "b": "response" }
  },
  "types": [
    "interface UserProfile { id: string; name: string; avatar: string; }"
  ]
}
```

### Package module
```json
{
  "project_id": 49,
  "module": { "name": "k9", "sem": "react", "cat": "pkg", "pkg": "react", "ver": "18.2.0" },
  "symbols": { "createElement": "createElement:1", "useState": "useState:1" }
}
```

## Error Handling

### Name Conflict
If the semantic name is already used by another module, the call fails with an error like:
```
Semantic name 'config/dotfile-patterns' is already used by module 'aYA'
```
Fix: choose an alternative name (e.g., `config/special-dotfiles`).

### Sibling Error Cascade
When submitting multiple modules in parallel, if ANY one fails, ALL sibling calls in the same batch fail. To mitigate:
- Submit in batches of 5-6 (not 10+)
- On cascade failure, resubmit the failed modules individually
