export default function(x) {
  x.a;
  x.a?.();
  x?.a;
  x?.a();
  x?.a.b;
  x?.a.b();
  x?.["foo_"].b;
  x?.a["bar_"];
}
