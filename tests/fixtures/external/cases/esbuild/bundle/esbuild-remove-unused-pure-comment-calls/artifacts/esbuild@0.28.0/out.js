(() => {
  // input/entry.js
  function bar() {
  }
  var bare = foo(bar);
  var at_no = /* @__PURE__ */ foo(bar());
  var new_at_no = /* @__PURE__ */ new foo(bar());
  var nospace_at_no = /* @__PURE__ */ foo(bar());
  var nospace_new_at_no = /* @__PURE__ */ new foo(bar());
  var num_no = /* @__PURE__ */ foo(bar());
  var new_num_no = /* @__PURE__ */ new foo(bar());
  var nospace_num_no = /* @__PURE__ */ foo(bar());
  var nospace_new_num_no = /* @__PURE__ */ new foo(bar());
  var dot_no = /* @__PURE__ */ foo(sideEffect()).dot(bar());
  var new_dot_no = /* @__PURE__ */ new foo(sideEffect()).dot(bar());
  var nested_no = [1, /* @__PURE__ */ foo(bar()), 2];
  var new_nested_no = [1, /* @__PURE__ */ new foo(bar()), 2];
  var single_at_no = /* @__PURE__ */ foo(bar());
  var new_single_at_no = /* @__PURE__ */ new foo(bar());
  var single_num_no = /* @__PURE__ */ foo(bar());
  var new_single_num_no = /* @__PURE__ */ new foo(bar());
  var bad_no = (
    /* __PURE__ */
    foo(bar)
  );
  var new_bad_no = (
    /* __PURE__ */
    new foo(bar)
  );
  var parens_no = foo(bar);
  var new_parens_no = new foo(bar);
  var exp_no = /* @__PURE__ */ foo() ** foo();
  var new_exp_no = /* @__PURE__ */ new foo() ** foo();
})();
