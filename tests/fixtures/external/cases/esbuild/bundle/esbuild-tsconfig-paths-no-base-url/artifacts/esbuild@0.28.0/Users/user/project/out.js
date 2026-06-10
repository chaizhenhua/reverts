(() => {
  // input/Users/user/project/simple/test0-success.ts
  var test0_success_default = "test0-success";

  // input/Users/user/project/simple/test1-success.ts
  var test1_success_default = "test1-success";

  // input/Users/user/project/simple/test2-success/foo.ts
  var foo_default = "test2-success";

  // input/Users/user/project/simple/test3-success.ts
  var test3_success_default = "test3-success";

  // input/Users/user/project/simple/test4-first/foo.ts
  var foo_default2 = "test4-success";

  // input/Users/user/project/simple/test5-second/foo.ts
  var foo_default3 = "test5-success";

  // input/Users/user/project/simple/actual/test.ts
  var test_default = "absolute-success";

  // input/Users/user/project/simple/index.ts
  var simple_default = {
    test0: test0_success_default,
    test1: test1_success_default,
    test2: foo_default,
    test3: test3_success_default,
    test4: foo_default2,
    test5: foo_default3,
    absolute: test_default
  };

  // input/Users/user/project/extended/nested/test0-success.ts
  var test0_success_default2 = "test0-success";

  // input/Users/user/project/extended/nested/test1-success.ts
  var test1_success_default2 = "test1-success";

  // input/Users/user/project/extended/nested/test2-success/foo.ts
  var foo_default4 = "test2-success";

  // input/Users/user/project/extended/nested/test3-success.ts
  var test3_success_default2 = "test3-success";

  // input/Users/user/project/extended/nested/test4-first/foo.ts
  var foo_default5 = "test4-success";

  // input/Users/user/project/extended/nested/test5-second/foo.ts
  var foo_default6 = "test5-success";

  // input/Users/user/project/extended/nested/actual/test.ts
  var test_default2 = "absolute-success";

  // input/Users/user/project/extended/index.ts
  var extended_default = {
    test0: test0_success_default2,
    test1: test1_success_default2,
    test2: foo_default4,
    test3: test3_success_default2,
    test4: foo_default5,
    test5: foo_default6,
    absolute: test_default2
  };

  // input/Users/user/project/entry.ts
  console.log(simple_default, extended_default);
})();
