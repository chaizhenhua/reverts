var obj2 = {};
var sideEffects2 = console.log("this should be renamed");
console.log("This is unused but still has side effects");
var replace2 = {
  test() {
  }
};
var replaceDot = {
  test() {
  }
};
import { re_export as re_export2 } from "external-pkg";
import { "reexpo.rt" as reexpo_rt } from "external-pkg2";
let sideEffects = console.log("side effects");
let collide = 123;
console.log(obj2.prop);
console.log("defined");
console.log("should be used");
console.log("should be used");
console.log(replace2.test);
console.log(replaceDot.test);
console.log(collide);
console.log(re_export2);
console.log(reexpo_rt);
