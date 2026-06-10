async function test() {
	const dep = await import('./chunks/dep.js');
	console.log(dep.obj.a.a.a);
}

test();
