import { f as fooPrefix } from './chunks/foo-prefix.js';

const { foo } = await import('./chunks/foo.js');

function getFoo() {
	return unknownFlag ? foo : fooPrefix;
}

console.log(getFoo());
