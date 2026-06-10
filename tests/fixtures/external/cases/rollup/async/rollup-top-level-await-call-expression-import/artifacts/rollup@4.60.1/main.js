import { g as getInfo } from './chunks/lib.js';

let getCommonInfo = getInfo;

function wrapper(cb) {
	return cb();
}

const { getInfoWithVariant } = await wrapper(() => import('./chunks/lib-variant.js'));
getCommonInfo = getInfoWithVariant;

export { getCommonInfo };
