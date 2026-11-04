import { api } from './api';

const THEMES = ['kissa-dark', 'kissa-light'] as const;
type Theme = (typeof THEMES)[number];

class ThemeStore {
  current = $state<Theme>('kissa-dark');

  async init() {
    try {
      const saved = await api.getSetting('ui.theme');
      if (saved && (THEMES as readonly string[]).includes(saved)) this.apply(saved as Theme);
    } catch {
      /* keep default */
    }
  }

  apply(t: Theme) {
    this.current = t;
    document.documentElement.setAttribute('data-theme', t);
  }

  async toggle() {
    const next = this.current === 'kissa-dark' ? 'kissa-light' : 'kissa-dark';
    this.apply(next);
    await api.setSetting('ui.theme', next).catch(() => {});
  }
}

export const theme = new ThemeStore();
