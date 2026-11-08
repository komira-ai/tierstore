// Tabs: makes every `.tabs[data-group]` of the page interactive (tab-logic.ts). Progressive enhancement: without this
// script every panel is on the page, one under the other.

import { enhance, storageOf } from './tab-logic.js';

const storage = storageOf(window);
for (const group of document.querySelectorAll<HTMLElement>('.tabs[data-group]')) enhance(group, storage);
