(() => {
  // input/entry.jsx
  var obj = {
    before,
    [key]: value,
    key: value,
    after
  };
  <Foo
    before
    {...{ [key]: value }}
    key={value}
    after
  />;
  <Bar
    a={a}
    {...{ [b]: c }}
    {...d}
    e={e}
  />;
})();
