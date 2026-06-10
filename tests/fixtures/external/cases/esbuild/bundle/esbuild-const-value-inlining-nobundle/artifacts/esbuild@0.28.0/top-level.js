const n_keep = null, u_keep = void 0, i_keep = 1234567, f_keep = 123.456, s_keep = "abc";
console.log(
  // These are doubled to avoid the "inline const/let into next statement if used once" optimization
  null,
  null,
  void 0,
  void 0,
  1234567,
  1234567,
  123.456,
  123.456,
  "abc",
  "abc"
);
