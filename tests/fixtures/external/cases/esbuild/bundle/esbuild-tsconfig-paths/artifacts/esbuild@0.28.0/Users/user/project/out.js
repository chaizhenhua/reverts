(() => {
  // Users/user/project/baseurl_dot/test0-success.ts
  var test0_success_default = "test0-success";

  // Users/user/project/baseurl_dot/test1-success.ts
  var test1_success_default = "test1-success";

  // Users/user/project/baseurl_dot/test2-success/foo.ts
  var foo_default = "test2-success";

  // Users/user/project/baseurl_dot/test3-success.ts
  var test3_success_default = "test3-success";

  // Users/user/project/baseurl_dot/test4-first/foo.ts
  var foo_default2 = "test4-success";

  // Users/user/project/baseurl_dot/test5-second/foo.ts
  var foo_default3 = "test5-success";

  // Users/user/project/baseurl_dot/actual/test.ts
  var test_default = "absolute-success";

  // Users/user/project/baseurl_dot/index.ts
  var baseurl_dot_default = {
    test0: test0_success_default,
    test1: test1_success_default,
    test2: foo_default,
    test3: test3_success_default,
    test4: foo_default2,
    test5: foo_default3,
    absoluteIn: test_default,
    absoluteInStar: test_default,
    absoluteOut: test_default,
    absoluteOutStar: test_default
  };

  // Users/user/project/baseurl_nested/nested/test0-success.ts
  var test0_success_default2 = "test0-success";

  // Users/user/project/baseurl_nested/nested/test1-success.ts
  var test1_success_default2 = "test1-success";

  // Users/user/project/baseurl_nested/nested/test2-success/foo.ts
  var foo_default4 = "test2-success";

  // Users/user/project/baseurl_nested/nested/test3-success.ts
  var test3_success_default2 = "test3-success";

  // Users/user/project/baseurl_nested/nested/test4-first/foo.ts
  var foo_default5 = "test4-success";

  // Users/user/project/baseurl_nested/nested/test5-second/foo.ts
  var foo_default6 = "test5-success";

  // Users/user/project/baseurl_nested/nested/actual/test.ts
  var test_default2 = "absolute-success";

  // Users/user/project/baseurl_nested/index.ts
  var baseurl_nested_default = {
    test0: test0_success_default2,
    test1: test1_success_default2,
    test2: foo_default4,
    test3: test3_success_default2,
    test4: foo_default5,
    test5: foo_default6,
    absoluteIn: test_default2,
    absoluteInStar: test_default2,
    absoluteOut: test_default2,
    absoluteOutStar: test_default2
  };

  // Users/user/project/entry.ts
  console.log(baseurl_dot_default, baseurl_nested_default);
})();
