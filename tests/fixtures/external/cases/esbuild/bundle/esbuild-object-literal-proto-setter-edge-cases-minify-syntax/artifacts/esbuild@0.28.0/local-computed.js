function foo(__proto__, bar) {
  {
    let __proto__2, bar2;
    console.log(
      'this must not become "{ __proto__: ... }":',
      {
        ["__proto__"]: __proto__2,
        bar: bar2
      }
    );
  }
}
