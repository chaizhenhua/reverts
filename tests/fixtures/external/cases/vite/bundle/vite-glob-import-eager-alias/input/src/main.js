export const eagerModules = import.meta.glob('@dir/*.js', { eager: true });
export const withoutX = import.meta.glob(['@dir/*.js', '!@dir/x.js'], { eager: true });