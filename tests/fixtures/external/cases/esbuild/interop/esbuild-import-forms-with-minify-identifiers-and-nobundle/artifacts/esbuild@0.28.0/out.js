import "foo";
import {} from "foo";
import * as o from "foo";
import { a as r, b as m } from "foo";
import t from "foo";
import f, * as i from "foo";
import p, { a2 as s, b as n } from "foo";
const a = [
  import("foo"),
  function() {
    return import("foo");
  }
];
console.log(o, r, m, t, f, i, p, s, n, a);
