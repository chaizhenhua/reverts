let value = 0;
await new Promise(r => setTimeout(r, 1));
value = 42;
console.log(value);
export {};