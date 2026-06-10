import { data } from "./cjs.js";
import * as star from "./cjs.js";
import def from "./cjs.js";
import { ns, default as def1, def as def2, data as data2 } from "./reexport.mjs";
import * as reexport from "./reexport.mjs";
import { data as flaggedData } from "./flagged.js";
import * as flaggedStar from "./flagged.js";
import flaggedDef from "./flagged.js";
import { data as dynamicData } from "./dynamic.js";
import * as dynamicStar from "./dynamic.js";
import dynamicDef from "./dynamic.js";
import { data as dynamicFlaggedData } from "./dynamicFlagged.js";
import * as dynamicFlaggedStar from "./dynamicFlagged.js";
import dynamicFlaggedDef from "./dynamicFlagged.js";
console.log(
	data, star.default, def, ns, def1, def2, data2, reexport,
	flaggedData, flaggedStar.default, flaggedDef,
	dynamicData, dynamicStar.default, dynamicDef,
	dynamicFlaggedData, dynamicFlaggedStar.default, dynamicFlaggedDef
);
export {
	data, star, def, ns, def1, def2, data2, reexport,
	flaggedData, flaggedStar, flaggedDef,
	dynamicData, dynamicStar, dynamicDef,
	dynamicFlaggedData, dynamicFlaggedStar, dynamicFlaggedDef
};