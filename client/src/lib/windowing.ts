import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow';
import { getJson, setJson } from './api';

/** `main` for the main window, `pop-1`, `pop-2`, ... for extra windows. */
export const windowLabel: string = (() => {
  try {
    return getCurrentWebviewWindow().label;
  } catch {
    return 'main';
  }
})();

/** True in an extra window: one page, no sidebar. */
export const isPopout = windowLabel !== 'main';

/**
 * Per-window settings. The main window uses `key` as is; an extra window keeps
 * its own copy under `key@pop-N`, so windows showing the same page do not
 * overwrite each other. Until it has saved its own value it starts from the
 * main window's.
 */
const scoped = (key: string) => (isPopout ? `${key}@${windowLabel}` : key);

export async function getScoped<T>(key: string, fallback: T): Promise<T> {
  if (isPopout) {
    const own = await getJson<T | null>(scoped(key), null);
    if (own !== null) return own;
  }
  return getJson<T>(key, fallback);
}

export const setScoped = (key: string, value: unknown) => setJson(scoped(key), value);
