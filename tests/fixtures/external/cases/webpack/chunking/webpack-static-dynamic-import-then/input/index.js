import("./dir1/a").then(m => {
	console.log(m.default, m.usedExports);
});
import("./dir4/lib").then(m => {
	console.log(m.b.f(), m.b.usedExports, m.usedExports);
	return import("./dir4/a").then(m2 => console.log(m2.a, m2.usedExports));
});