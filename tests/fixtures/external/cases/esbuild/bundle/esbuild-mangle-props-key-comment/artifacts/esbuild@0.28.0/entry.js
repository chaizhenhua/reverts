x(
  /* __KEY__ */
  "_doNotMangleThis",
  /* __KEY__ */
  `_doNotMangleThis`
);
x.a(/* @__KEY__ */ "a", /* @__KEY__ */ "a");
x.b(/* @__KEY__ */ "b", /* @__KEY__ */ "b");
x.c = /* @__KEY__ */ "c" in y;
x([
  `foo.${/* @__KEY__ */ "a"} = bar.${/* @__KEY__ */ "b"}`,
  `foo.${/* @__KEY__ */ "notMangled"} = bar.${/* @__KEY__ */ "notMangledEither"}`
]);
