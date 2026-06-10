Promise.all([
  import("./data/a.json"),
  import("./data/b.json"),
  import("./data/c.json"),
  import("./data/d.json"),
  import("./data/e.json"),
  import("./data/f.json"),
  import("./data/g.json"),
]).then(values => console.log(values));
