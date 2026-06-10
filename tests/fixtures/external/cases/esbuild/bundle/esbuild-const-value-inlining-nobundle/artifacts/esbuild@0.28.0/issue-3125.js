function foo() {
  const f = () => x, x = 0;
  return f();
}
