(() => {
  // input/entry.ts
  inlined = [
    obj.abc,
    obj.xyz,
    obj?.abc,
    obj?.xyz,
    obj?.prop.abc,
    obj?.prop.xyz
  ];
  notInlined = [
    obj["a b c" /* foo2 */],
    obj["x y z" /* bar2 */],
    obj?.["a b c" /* foo2 */],
    obj?.["x y z" /* bar2 */],
    obj?.prop["a b c" /* foo2 */],
    obj?.prop["x y z" /* bar2 */]
  ];
})();
