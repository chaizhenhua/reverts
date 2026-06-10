var __glob = (map) => (path) => {
  var fn = map[path];
  if (fn) return fn();
  throw new Error("Module not found in bundle: " + path);
};

// entry.js
import source foo0 from "./foo.json";
import source foo1 from "./foo.json" with { type: "json" };

// import.source("./**/*.json") in entry.js
var globImport_json = __glob({
  "./foo.json": () => import.source("./foo.json")
});

// import.source("./**/*.json") in entry.js
var globImport_json2 = __glob({
  "./foo.json": () => import.source("./foo.json", { with: { type: "json" } })
});

// entry.js
console.log(
  foo0,
  foo1,
  import.source("./foo.json"),
  import.source("./foo.json", { with: { type: "json" } }),
  globImport_json(`./${foo}.json`),
  globImport_json2(`./${foo}.json`)
);
