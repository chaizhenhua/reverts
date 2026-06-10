import lib, { foo } from "./lib.cjs";
console.log(JSON.stringify({ default: lib, named: foo }));
