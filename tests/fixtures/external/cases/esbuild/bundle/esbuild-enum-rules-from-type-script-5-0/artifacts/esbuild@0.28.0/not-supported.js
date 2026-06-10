(() => {
  // input/not-supported.ts
  var NonIntegerNumberToString = ((NonIntegerNumberToString2) => {
    NonIntegerNumberToString2["SUPPORTED"] = "1";
    NonIntegerNumberToString2["UNSUPPORTED"] = "" + 1.5;
    return NonIntegerNumberToString2;
  })(NonIntegerNumberToString || {});
  console.log(
    "1" /* SUPPORTED */,
    NonIntegerNumberToString.UNSUPPORTED
  );
  var OutOfBoundsNumberToString = ((OutOfBoundsNumberToString2) => {
    OutOfBoundsNumberToString2["SUPPORTED"] = "1000000000";
    OutOfBoundsNumberToString2["UNSUPPORTED"] = "" + 1e12;
    return OutOfBoundsNumberToString2;
  })(OutOfBoundsNumberToString || {});
  console.log(
    "1000000000" /* SUPPORTED */,
    OutOfBoundsNumberToString.UNSUPPORTED
  );
  console.log(
    "null" /* NULL */,
    "true" /* TRUE */,
    "false" /* FALSE */,
    "123" /* BIGINT */
  );
})();
