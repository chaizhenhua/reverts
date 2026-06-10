export function a(o = foo) {
  var r;
  return o;
}
export class b {
  fn(r = foo) {
    var f;
    return r;
  }
}
export let c = [
  function(o = foo) {
    var r;
    return o;
  },
  (o = foo) => {
    var r;
    return o;
  },
  { fn(o = foo) {
    var r;
    return o;
  } },
  class {
    fn(o = foo) {
      var r;
      return o;
    }
  }
];
