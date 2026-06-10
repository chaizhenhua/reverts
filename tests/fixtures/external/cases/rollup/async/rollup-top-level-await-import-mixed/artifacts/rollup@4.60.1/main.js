import { g as getInfo } from './chunks/lib.js';

let getCommonInfo = getInfo;

import('./chunks/lib-used.js').then(({ getInfoWithUsed }) => {
	getCommonInfo = getInfoWithUsed;
});

const { getInfoWithVariant } = await import('./chunks/lib-variant.js');
getCommonInfo = getInfoWithVariant;

export { getCommonInfo };
