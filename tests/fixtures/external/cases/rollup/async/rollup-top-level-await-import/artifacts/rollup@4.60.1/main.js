import { g as getInfo } from './chunks/lib.js';

let getCommonInfo = getInfo;

const { getInfoWithVariant } = await import('./chunks/lib-variant.js');
getCommonInfo = getInfoWithVariant;

export { getCommonInfo };
