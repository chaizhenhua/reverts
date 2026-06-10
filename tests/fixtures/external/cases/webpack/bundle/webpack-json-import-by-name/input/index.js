import * as c from "./c.json";
import { bb, aa } from "./e.json";
import f, { named } from "./f.json";
import g, { named as gnamed } from "./g.json";
console.log(c[2], aa, bb, named, g.named, gnamed, f.default);
export { c, aa, bb, f, named, g, gnamed };