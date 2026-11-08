// What the tabs do once the page has loaded (src/tabs.ts runs it): the WAI-ARIA tabs pattern over the markup tools/tabs.ts
// builds. Kept apart from the entry module so a test can drive it with stand-ins for the page.

/** The tab an arrow key, Home or End moves to, or null for any other key. The ends wrap. */
export function nextIndex(key: string, current: number, count: number): number | null {
  switch (key) {
    case 'ArrowRight':
    case 'ArrowDown':
      return (current + 1) % count;
    case 'ArrowLeft':
    case 'ArrowUp':
      return (current - 1 + count) % count;
    case 'Home':
      return 0;
    case 'End':
      return count - 1;
    default:
      return null;
  }
}

/** localStorage, or null where the browser refuses the page its storage (the access itself can throw). */
export function storageOf(win: { localStorage: Storage }): Storage | null {
  try {
    return win.localStorage;
  } catch {
    return null;
  }
}

const keyOf = (group: string): string => `tierstore.tab.${group}`;

/** The tab remembered for a group, or null: nothing stored, no storage, or a storage that throws. */
export function recall(storage: Pick<Storage, 'getItem'> | null, group: string): string | null {
  try {
    return storage === null ? null : storage.getItem(keyOf(group));
  } catch {
    return null;
  }
}

/** Remembers the choice; a full or refused storage is not an error. */
export function remember(storage: Pick<Storage, 'setItem'> | null, group: string, tab: string): void {
  try {
    storage?.setItem(keyOf(group), tab);
  } catch {
    // The page works the same without the memory.
  }
}

/** Makes one `.tabs[data-group]` interactive: shows its tablist, selects the remembered tab (or the first) and hides the other panels. */
export function enhance(group: HTMLElement, storage: Storage | null): void {
  const name = group.dataset.group ?? '';
  const tabs = [...group.querySelectorAll<HTMLElement>('[role="tab"]')];
  const panels = [...group.querySelectorAll<HTMLElement>('[role="tabpanel"]')];
  const list = group.querySelector<HTMLElement>('[role="tablist"]');
  if (tabs.length === 0 || tabs.length !== panels.length || list === null) return;
  const select = (index: number, focus: boolean, save: boolean): void => {
    tabs.forEach((t, i) => {
      t.setAttribute('aria-selected', String(i === index));
      t.tabIndex = i === index ? 0 : -1;
      panels[i].hidden = i !== index;
    });
    if (focus) tabs[index].focus();
    if (save) remember(storage, name, tabs[index].dataset.tab as string);
  };
  tabs.forEach((t, i) => {
    t.addEventListener('click', () => select(i, false, true));
    t.addEventListener('keydown', (e: KeyboardEvent) => {
      const to = nextIndex(e.key, i, tabs.length);
      if (to === null) return;
      e.preventDefault();
      select(to, true, true);
    });
  });
  const saved = recall(storage, name);
  const first = tabs.findIndex((t) => t.dataset.tab === saved);
  list.hidden = false;
  group.classList.add('tabs-on');
  select(Math.max(first, 0), false, false);
}
