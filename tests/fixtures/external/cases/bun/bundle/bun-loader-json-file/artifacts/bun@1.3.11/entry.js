// @bun
// tests/fixtures/external/cases/bun/bundle/bun-loader-json-file/input/hello.notjson
var hello_default = {
  hello: "world"
};

// tests/fixtures/external/cases/bun/bundle/bun-loader-json-file/input/entry.ts
console.write(JSON.stringify(hello_default));
