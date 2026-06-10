(() => {
  // input/enums.ts
  var a = ((a2) => {
    a2[a2["implicit_number"] = 0] = "implicit_number";
    a2[a2["explicit_number"] = 123] = "explicit_number";
    a2["explicit_string"] = "xyz";
    a2[a2["non_constant"] = foo] = "non_constant";
    return a2;
  })(a || {});

  // input/entry.ts
  console.log([
    0 /* implicit_number */,
    123 /* explicit_number */,
    "xyz" /* explicit_string */,
    a.non_constant
  ]);
})();
