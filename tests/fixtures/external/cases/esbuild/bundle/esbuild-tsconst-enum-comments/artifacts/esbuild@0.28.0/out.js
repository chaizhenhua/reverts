(() => {
  // input/foo.ts
  console.log({
    "should have comments": [
      1 /* %/* */,
      1 /* %/* */
    ],
    "should not have comments": [
      2,
      2
    ]
  });
})();
