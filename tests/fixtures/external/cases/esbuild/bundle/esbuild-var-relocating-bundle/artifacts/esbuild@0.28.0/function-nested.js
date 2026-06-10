// function-nested.js
function x() {
  if (true) {
    let l2 = function() {
    };
    var l = l2;
    var a;
    for (var b; 0; ) ;
    for (var { c, x: [d] } = {}; 0; ) ;
    for (var e of []) ;
    for (var { f, x: [g] } of []) ;
    for (var h in {}) ;
    i = 1;
    for (var i in {}) ;
    for (var { j, x: [k] } in {}) ;
  }
}
x();
