Promise.all([
	import("./a.json"),
	import("./e.json"),
	import("./f.json")
]).then(([a, e, f]) => {
	console.log(a.default, e.default.aa, f.default.named);
});