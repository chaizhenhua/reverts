const { getInfoWithUsed } = await import('./lib-used.js');

function getInfoWithVariant() {
	return getInfoWithUsed() + '_variant';
}

export { getInfoWithVariant };
