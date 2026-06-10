var test = 123;
var test_default = { test, "invalid-identifier": true };
export {
  test_default as default,
  test
};
