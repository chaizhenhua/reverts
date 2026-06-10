(() => {
  let e = 1;
  let outer = 2;
  let outerDead = 3;
  with ({}) {
    var hoisted = 4;
    let t = 5;
    hoisted++;
    t++;
    if (1) outer++;
    if (0) outerDead++;
  }
  if (1) {
    hoisted++;
    e++;
    outer++;
    outerDead++;
  }
})();
