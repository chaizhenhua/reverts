import defaultExport from "./lib.cjs";
import { foo } from "./lib.cjs";
import * as namespace from "./lib.cjs";
console.log(JSON.stringify({
  default: defaultExport,
  named: foo,
  namespace: namespace
}));
