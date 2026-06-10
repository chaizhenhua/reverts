(() => {
  // <data:application/json,"%31%32%33">
  var json_31_32_33_default = "123";

  // <data:application/json;base64,eyJ3b3JrcyI6dHJ1ZX0=>
  var json_base64_eyJ3b3JrcyI6dHJ1ZX0_default = { works: true };

  // <data:application/json;charset=UTF-8,%31%32%33>
  var json_charset_UTF_8_31_32_33_default = 123;

  // <data:application/json;charset=UTF-8;base64,eyJ3b3JrcyI6dHJ1ZX0=>
  var json_charset_UTF_8_base64_eyJ3b3JrcyI6dHJ1ZX0_default = { works: true };

  // input/entry.js
  console.log([
    json_31_32_33_default,
    json_base64_eyJ3b3JrcyI6dHJ1ZX0_default,
    json_charset_UTF_8_31_32_33_default,
    json_charset_UTF_8_base64_eyJ3b3JrcyI6dHJ1ZX0_default
  ]);
})();
