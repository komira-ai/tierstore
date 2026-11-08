// Tabs without script: the copy writes
//
//   <div class="tabs" data-tabs="client"><section data-tab="Rust">...</section><section data-tab="Python">...</section></div>
//
// and the build turns it into radio inputs, their labels and the panels, which site.css shows one at a time. With no
// stylesheet every panel is on the page, in order; with one, a keyboard moves between the tabs with the arrow keys
// (the radios are real). A section holds no nested <section>.

import { escapeText } from './layout';

/** The most tabs one group has: site.css has a rule for each position. */
export const MAX_TABS = 6;

export function renderTabs(html: string): string {
  return html.replace(/<div class="tabs" data-tabs="([a-z0-9-]+)">\s*((?:<section data-tab="[^"]+">[\s\S]*?<\/section>\s*)+)<\/div>/g, (_m, group: string, body: string) => {
    const panels = [...body.matchAll(/<section data-tab="([^"]+)">([\s\S]*?)<\/section>/g)].map((m) => ({ label: m[1], html: m[2].trim() }));
    if (panels.length < 2 || panels.length > MAX_TABS) throw new Error(`tabs: group ${group} has ${panels.length} tabs, not 2 to ${MAX_TABS}`);
    const id = (i: number): string => `tab-${group}-${i + 1}`;
    return [
      `<div class="tabs" role="group" aria-label="${escapeText(group.replace(/-/g, ' '))}">`,
      ...panels.map((_p, i) => `<input type="radio" class="tab-input" name="tab-${group}" id="${id(i)}"${i === 0 ? ' checked' : ''}>`),
      `<div class="tab-list">${panels.map((p, i) => `<label for="${id(i)}">${p.label}</label>`).join('')}</div>`,
      `<div class="tab-panels">${panels.map((p) => `<div class="tab-panel">\n${p.html}\n</div>`).join('\n')}</div>`,
      '</div>',
    ].join('\n');
  });
}
