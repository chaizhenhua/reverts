var __glob = (map) => (path) => {
  var fn = map[path];
  if (fn) return fn();
  throw new Error("Module not found in bundle: " + path);
};

// entry.js
import defer * as foo0 from "./foo.json";
import defer * as foo1 from "./foo.json" with { type: "json" };

// import.defer("./**/*.json") in entry.js
var globImport_json = __glob({
  "./foo.json": () => import.defer("./foo.json")
});

// import.defer("./**/*.json") in entry.js
var globImport_json2 = __glob({
  "./foo.json": () => import.defer("./foo.json", { with: { type: "json" } })
});

// entry.js
console.log(
  foo0,
  foo1,
  import.defer("./foo.json"),
  import.defer("./foo.json", { with: { type: "json" } }),
  globImport_json(`./${foo}.json`),
  globImport_json2(`./${foo}.json`)
);
