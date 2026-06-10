import * as ns from "./reexport-async-unknown.js";
import { a, b, c } from "./reexport-async-unknown.js";
import value from "./reexport-async-unknown.js";
console.log(ns.default, a, b, c, value);
export { ns, a, b, c, value };