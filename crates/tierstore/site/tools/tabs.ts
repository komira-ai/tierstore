// Tabs: the copy writes
//
//   <div class="tabs" data-tabs="client"><section data-tab="Rust">...</section><section data-tab="Python">...</section></div>
//
// and the build turns it into a tablist of real buttons (role="tab") and a panel (role="tabpanel") for each section, as
// the WAI-ARIA tabs pattern has them. With no script every panel is on the page, one under the other, each under its
// own title, and the tablist is hidden; src/tabs.ts shows the tablist, hides all panels but the chosen one and moves
// between tabs with the arrow keys. A section holds no nested <section>.

import { escapeText } from './layout';

/** The most tabs one group has. */
export const MAX_TABS = 8;

/** A label as an id fragment and as the value a visitor's choice is remembered by: 'Python (boto3)' is 'python-boto3'. */
export function slug(label: string): string {
  return label
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-|-$/g, '');
}

export function renderTabs(html: string): string {
  return html.replace(/<div class="tabs" data-tabs="([a-z0-9-]+)">\s*((?:<section data-tab="[^"]+">[\s\S]*?<\/section>\s*)+)<\/div>/g, (_m, group: string, body: string) => {
    const panels = [...body.matchAll(/<section data-tab="([^"]+)">([\s\S]*?)<\/section>/g)].map((m) => ({ label: m[1], html: m[2].trim() }));
    if (panels.length < 2 || panels.length > MAX_TABS) throw new Error(`tabs: group ${group} has ${panels.length} tabs, not 2 to ${MAX_TABS}`);
    const slugs = panels.map((p) => slug(p.label));
    if (new Set(slugs).size !== slugs.length || slugs.includes('')) throw new Error(`tabs: group ${group} has labels that are empty or that make one id twice`);
    const tab = (i: number): string => `tab-${group}-${slugs[i]}`;
    const panel = (i: number): string => `panel-${group}-${slugs[i]}`;
    const name = group.replace(/-/g, ' ');
    return [
      `<div class="tabs" data-group="${group}">`,
      `<div class="tab-list" role="tablist" aria-label="${escapeText(name)}" hidden>`,
      ...panels.map(
        (p, i) =>
          `<button type="button" role="tab" id="${tab(i)}" aria-controls="${panel(i)}" aria-selected="${i === 0}" tabindex="${i === 0 ? 0 : -1}" data-tab="${slugs[i]}">${p.label}</button>`,
      ),
      '</div>',
      ...panels.map((p, i) => `<div class="tab-panel" role="tabpanel" id="${panel(i)}" aria-labelledby="${tab(i)}" tabindex="0">\n<p class="tab-title">${p.label}</p>\n${p.html}\n</div>`),
      '</div>',
    ].join('\n');
  });
}
