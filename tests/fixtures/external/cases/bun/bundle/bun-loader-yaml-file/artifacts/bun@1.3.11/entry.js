// @bun
// tests/fixtures/external/cases/bun/bundle/bun-loader-yaml-file/input/hello.notyaml
var hello_default = {
  hello: "world"
};

// tests/fixtures/external/cases/bun/bundle/bun-loader-yaml-file/input/entry.ts
console.write(JSON.stringify(hello_default));
