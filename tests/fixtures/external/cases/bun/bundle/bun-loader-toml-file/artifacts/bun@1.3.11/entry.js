// @bun
// tests/fixtures/external/cases/bun/bundle/bun-loader-toml-file/input/hello.nottoml
var hello_default = {
  hello: "world"
};

// tests/fixtures/external/cases/bun/bundle/bun-loader-toml-file/input/entry.ts
console.write(JSON.stringify(hello_default));
