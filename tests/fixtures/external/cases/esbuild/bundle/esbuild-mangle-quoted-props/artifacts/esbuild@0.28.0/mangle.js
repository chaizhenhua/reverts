x.a;
x?.a;
x[y ? "a" : z];
x?.[y ? "a" : z];
x[y ? z : "a"];
x?.[y ? z : "a"];
x[y, "a"];
x?.[y, "a"];
({ a: x });
({ ["a"]: x });
({ [(y, "a")]: x });
(class {
  a = x;
});
(class {
  ["a"] = x;
});
(class {
  [(y, "a")] = x;
});
var { a: x } = y;
var { ["a"]: x } = y;
var { [(z, "a")]: x } = y;
"a" in x;
(y ? "a" : z) in x;
(y ? z : "a") in x;
(y, "a") in x;
