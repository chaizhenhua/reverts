export const modules = import.meta.glob('./dir/*.js');
export async function loadAll() {
  const loaded = {};
  for (const [key, loader] of Object.entries(modules)) {
    loaded[key] = (await loader()).default;
  }
  return loaded;
}