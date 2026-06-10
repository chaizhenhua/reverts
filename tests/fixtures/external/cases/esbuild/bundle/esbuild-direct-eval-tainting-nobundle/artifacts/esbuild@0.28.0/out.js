function test1() {
  function add(n, t) {
    return n + t;
  }
  eval("add(1, 2)");
}
function test2() {
  function n(t, e) {
    return t + e;
  }
  (0, eval)("add(1, 2)");
}
function test3() {
  function n(t, e) {
    return t + e;
  }
}
function test4(eval) {
  function add(n, t) {
    return n + t;
  }
  eval("add(1, 2)");
}
function test5() {
  function containsDirectEval() {
    eval();
  }
  if (true) {
    var shouldNotBeRenamed;
  }
}
