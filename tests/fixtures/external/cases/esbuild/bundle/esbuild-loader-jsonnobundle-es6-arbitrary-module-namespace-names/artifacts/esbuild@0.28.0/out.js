var test = 123;
var invalid_identifier = true;
var test_default = { test, "invalid-identifier": invalid_identifier };
export {
  test_default as default,
  invalid_identifier as "invalid-identifier",
  test
};
