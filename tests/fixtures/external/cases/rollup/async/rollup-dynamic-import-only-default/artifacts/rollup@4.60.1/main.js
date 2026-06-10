var main = Promise.all([import('./entry.js'), import('./chunks/other.js')]);

export { main as default };
