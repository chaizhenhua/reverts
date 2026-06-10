function nested() {
  const x = [, "", {}, 0n, /./, function() {
  }, () => {
  }];
  function foo() {
    return 1;
  }
}
