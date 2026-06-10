{
  const s_keep = "Long strings are not inlined as constants";
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
    "abc",
    s_keep,
    s_keep
  );
}
