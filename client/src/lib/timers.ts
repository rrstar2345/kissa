/**
 * `setInterval` that skips its callback while the window is hidden or
 * minimised, so background polling costs nothing. Returns the interval id
 * (use `clearInterval`).
 */
export function every(fn: () => void, ms: number): ReturnType<typeof setInterval> {
  return setInterval(() => {
    if (!document.hidden) fn();
  }, ms);
}
