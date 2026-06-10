(() => {
  // input/entry.js
  function testReturn() {
    return y + z();
    if (x)
      var y;
    function z() {
      KEEP_ME();
    }
  }
  function testThrow() {
    throw y + z();
    if (x)
      var y;
    function z() {
      KEEP_ME();
    }
  }
  function testBreak() {
    for (; ; ) {
      let z2 = function() {
        KEEP_ME();
      };
      var z = z2;
      y + z2();
      break;
      if (x)
        var y;
    }
  }
  function testContinue() {
    for (; ; ) {
      let z2 = function() {
        KEEP_ME();
      };
      var z = z2;
      y + z2();
      continue;
      if (x)
        var y;
    }
  }
  function testStmts() {
    return [a, b, c, d, e, f, g, h, i];
    for (; x; )
      var a;
    do
      var b;
    while (x);
    for (var c; ; ) ;
    for (var d in x) ;
    for (var e of x) ;
    if (x)
      var f;
    if (!x) var g;
    var h, i;
  }
  testReturn();
  testThrow();
  testBreak();
  testContinue();
  testStmts();
})();
