function empty() {
}
function id(x) {
  return x;
}
export let shouldBeWrapped = [
  (0, foo.bar)(),
  (0, foo[bar])(),
  (0, foo?.bar)(),
  (0, foo?.[bar])(),
  (0, foo.bar)(),
  (0, foo[bar])(),
  (0, foo?.bar)(),
  (0, foo?.[bar])(),
  (0, eval)(),
  (0, eval)?.(),
  (0, eval)(),
  (0, eval)?.(),
  (0, foo.bar)``,
  (0, foo[bar])``,
  (0, foo?.bar)``,
  (0, foo?.[bar])``,
  (0, foo.bar)``,
  (0, foo[bar])``,
  (0, foo?.bar)``,
  (0, foo?.[bar])``,
  delete (0, foo),
  delete (0, foo.bar),
  delete (0, foo[bar]),
  delete (0, foo?.bar),
  delete (0, foo?.[bar]),
  delete (0, foo),
  delete (0, foo.bar),
  delete (0, foo[bar]),
  delete (0, foo?.bar),
  delete (0, foo?.[bar]),
  delete (0, void 0)
], shouldNotBeWrapped = [
  foo(),
  foo(),
  foo``,
  foo``
], shouldNotBeDoubleWrapped = [
  delete (foo(), bar()),
  delete (foo(), bar())
];
