import("./dir1/a").then(m => {
	console.log(m.default, m.usedExports);
});
import("./dir4/a").then(m => {
	console.log(m.a, m.f(), m.usedExports);
});