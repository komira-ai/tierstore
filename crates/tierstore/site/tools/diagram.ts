// Diagrams, drawn when the pages are built: a ```diagram fence of a docs page is a small description (boxes on a grid,
// arrows between them) and comes out as an inline SVG in the site's look. No script, no style attribute, no ids, no
// dependency; the CSS classes are in static/site.css.
//
//   alt: <one sentence that says what the picture shows; it is the caption under the picture, which a screen reader reads>
//   node <id> <column 0-2> <row, in halves> "<label>" "<second line>" [main|hot|remote|ghost]
//   edge <from> <to> "<label>" [both] [dashed]
//
// A node is a box on the grid (three columns, rows of any number of halves). An edge is an arrow from one box to
// another: sideways between columns, up or down within a column; `both` has a head at each end, `dashed` marks what
// happens in the background (a copy, an eviction). The description is refused if a box is off the grid, a name is used
// twice or an arrow names no box, so a diagram that cannot be drawn is a build error.

import { escapeText } from './layout';

export type NodeKind = 'plain' | 'main' | 'hot' | 'remote' | 'ghost';
const KINDS: readonly NodeKind[] = ['plain', 'main', 'hot', 'remote', 'ghost'];

export interface DiagramNode {
  id: string;
  col: number;
  row: number;
  label: string;
  sub: string;
  kind: NodeKind;
}
export interface DiagramEdge {
  from: string;
  to: string;
  label: string;
  both: boolean;
  dashed: boolean;
}
export interface Diagram {
  alt: string;
  nodes: DiagramNode[];
  edges: DiagramEdge[];
}

/** The most characters a box's label and second line may have: what fits the box at the size they are drawn. */
export const MAX_LABEL = 12;
export const MAX_SUB = 16;
/** The most characters an arrow's label may have: what fits between two columns. */
export const MAX_EDGE_LABEL = 12;
export const COLUMNS = 3;

/** A line split into words, where "a quoted string" is one word. */
function words(line: string, name: string): string[] {
  const out: string[] = [];
  const re = /\s*(?:"([^"]*)"|(\S+))/gy;
  let pos = 0;
  while (pos < line.length) {
    re.lastIndex = pos;
    const m = re.exec(line);
    if (m === null) {
      if (line.slice(pos).trim() === '') break;
      throw new Error(`diagram: ${name}: cannot read "${line}"`);
    }
    out.push(m[1] ?? m[2]);
    pos = re.lastIndex;
  }
  return out;
}

export function parseDiagram(src: string, name = 'a diagram'): Diagram {
  const d: Diagram = { alt: '', nodes: [], edges: [] };
  for (const raw of src.split('\n')) {
    const line = raw.trim();
    if (line === '') continue;
    const alt = /^alt:\s*(.+)$/.exec(line);
    if (alt !== null) {
      if (d.alt !== '') throw new Error(`diagram: ${name}: two alt lines`);
      d.alt = alt[1];
      continue;
    }
    const w = words(line, name);
    if (w[0] === 'node') {
      if (w.length < 6 || w.length > 7) throw new Error(`diagram: ${name}: a node is: node id column row "label" "second line" [kind], not "${line}"`);
      const [, id, col, row, label, sub, kind = 'plain'] = w;
      const c = Number(col);
      const r = Number(row);
      if (!Number.isInteger(c) || c < 0 || c >= COLUMNS) throw new Error(`diagram: ${name}: node ${id} is in column ${col}, not 0 to ${COLUMNS - 1}`);
      if (!Number.isFinite(r) || r < 0 || r * 2 !== Math.round(r * 2)) throw new Error(`diagram: ${name}: node ${id} is in row ${row}, not a number of halves from 0`);
      if (!/^[a-z][a-z0-9-]*$/.test(id) || d.nodes.some((n) => n.id === id)) throw new Error(`diagram: ${name}: node name ${id} is not a lower-case word or is used twice`);
      if (!KINDS.includes(kind as NodeKind)) throw new Error(`diagram: ${name}: node ${id} is of kind ${kind}, not one of ${KINDS.join(', ')}`);
      if (label.length > MAX_LABEL || sub.length > MAX_SUB) throw new Error(`diagram: ${name}: node ${id} has words too long for its box (${MAX_LABEL} and ${MAX_SUB} characters)`);
      if (d.nodes.some((n) => n.col === c && n.row === r)) throw new Error(`diagram: ${name}: node ${id} is on the place of another`);
      d.nodes.push({ id, col: c, row: r, label, sub, kind: kind as NodeKind });
    } else if (w[0] === 'edge') {
      if (w.length < 4) throw new Error(`diagram: ${name}: an edge is: edge from to "label" [both] [dashed], not "${line}"`);
      const [, from, to, label, ...flags] = w;
      for (const f of flags) if (f !== 'both' && f !== 'dashed') throw new Error(`diagram: ${name}: an edge flag is both or dashed, not ${f}`);
      if (label.length > MAX_EDGE_LABEL) throw new Error(`diagram: ${name}: the label "${label}" is longer than ${MAX_EDGE_LABEL} characters`);
      d.edges.push({ from, to, label, both: flags.includes('both'), dashed: flags.includes('dashed') });
    } else {
      throw new Error(`diagram: ${name}: cannot read "${line}"`);
    }
  }
  if (d.alt === '') throw new Error(`diagram: ${name}: no alt line`);
  if (d.nodes.length === 0) throw new Error(`diagram: ${name}: no nodes`);
  for (const e of d.edges) {
    for (const end of [e.from, e.to]) if (!d.nodes.some((n) => n.id === end)) throw new Error(`diagram: ${name}: an edge names ${end}, which is no node`);
    if (e.from === e.to) throw new Error(`diagram: ${name}: an edge from ${e.from} to itself`);
  }
  return d;
}

// The grid, in the units of the viewBox.
const BOX_W = 120;
const BOX_H = 62;
const COL_GAP = 84;
const ROW_GAP = 44;
const PAD = 8;
const HEAD = 8;

const num = (n: number): string => String(Math.round(n * 10) / 10);

/** The size of the picture's viewBox, from the grid the nodes use. */
export function sizeOf(d: Diagram): { w: number; h: number } {
  const rows = Math.max(...d.nodes.map((n) => n.row));
  return { w: PAD * 2 + COLUMNS * BOX_W + (COLUMNS - 1) * COL_GAP, h: PAD * 2 + BOX_H + rows * (BOX_H + ROW_GAP) };
}

/** An arrowhead whose tip is at (x, y) and that points along (dx, dy), a unit vector. */
function head(x: number, y: number, dx: number, dy: number): string {
  const bx = x - dx * HEAD;
  const by = y - dy * HEAD;
  const px = -dy * (HEAD / 2);
  const py = dx * (HEAD / 2);
  return `<polygon class="dg-head" points="${num(x)},${num(y)} ${num(bx + px)},${num(by + py)} ${num(bx - px)},${num(by - py)}"></polygon>`;
}

export function renderDiagram(d: Diagram, caption = true): string {
  const { w, h } = sizeOf(d);
  const at = (n: DiagramNode): { x: number; y: number } => ({ x: PAD + n.col * (BOX_W + COL_GAP), y: PAD + n.row * (BOX_H + ROW_GAP) });
  const byId = new Map(d.nodes.map((n) => [n.id, n]));
  const lines: string[] = [];
  const labels: string[] = [];
  for (const e of d.edges) {
    const a = byId.get(e.from) as DiagramNode;
    const b = byId.get(e.to) as DiagramNode;
    const pa = at(a);
    const pb = at(b);
    let x1: number;
    let y1: number;
    let x2: number;
    let y2: number;
    let vertical = false;
    if (a.col === b.col) {
      vertical = true;
      x1 = pa.x + BOX_W / 2;
      x2 = x1;
      const down = b.row > a.row;
      y1 = down ? pa.y + BOX_H : pa.y;
      y2 = down ? pb.y : pb.y + BOX_H;
    } else {
      const right = b.col > a.col;
      x1 = right ? pa.x + BOX_W : pa.x;
      x2 = right ? pb.x : pb.x + BOX_W;
      y1 = pa.y + BOX_H / 2;
      y2 = pb.y + BOX_H / 2;
    }
    const len = Math.hypot(x2 - x1, y2 - y1);
    const dx = (x2 - x1) / len;
    const dy = (y2 - y1) / len;
    const gap = 2;
    // The line stops where the head starts, so the line does not show through it.
    const sx = x1 + (e.both ? dx * (HEAD + gap) : dx * gap);
    const sy = y1 + (e.both ? dy * (HEAD + gap) : dy * gap);
    const ex = x2 - dx * (HEAD + gap);
    const ey = y2 - dy * (HEAD + gap);
    const cls = e.dashed ? 'dg-line dg-dashed' : 'dg-line';
    lines.push(`<g class="${e.dashed ? 'dg-edge dg-edge-dashed' : 'dg-edge'}"><path class="${cls}" d="M${num(sx)} ${num(sy)} L${num(ex)} ${num(ey)}"></path>${head(x2 - dx * gap, y2 - dy * gap, dx, dy)}${e.both ? head(x1 + dx * gap, y1 + dy * gap, -dx, -dy) : ''}</g>`);
    const mx = (x1 + x2) / 2;
    const my = (y1 + y2) / 2;
    labels.push(vertical ? `<text class="dg-edge-label" x="${num(mx + 9)}" y="${num(my + 5)}">${escapeText(e.label)}</text>` : `<text class="dg-edge-label" x="${num(mx)}" y="${num(my - 7)}" text-anchor="middle">${escapeText(e.label)}</text>`);
  }
  const boxes = d.nodes.map((n) => {
    const p = at(n);
    const cx = p.x + BOX_W / 2;
    return [
      `<g class="dg-node dg-${n.kind}">`,
      `<rect x="${num(p.x)}" y="${num(p.y)}" width="${BOX_W}" height="${BOX_H}" rx="4"></rect>`,
      `<text class="dg-name" x="${num(cx)}" y="${num(p.y + 27)}" text-anchor="middle">${escapeText(n.label)}</text>`,
      `<text class="dg-sub" x="${num(cx)}" y="${num(p.y + 47)}" text-anchor="middle">${escapeText(n.sub)}</text>`,
      '</g>',
    ].join('');
  });
  return [
    `<figure class="diagram"><svg class="diagram-svg" viewBox="0 0 ${w} ${h}" aria-hidden="true" focusable="false">`,
    ...lines,
    ...boxes,
    ...labels,
    `</svg>${caption ? `<figcaption>${escapeText(d.alt)}</figcaption>` : ''}</figure>`,
  ].join('\n');
}

/** The picture as text, for the raw Markdown of a page: what the alt line says and each arrow on a line of its own. */
export function describeDiagram(d: Diagram): string {
  const name = (id: string): string => (byName(d, id) as DiagramNode).label;
  const arrows = d.edges.map((e) => `- ${name(e.from)} ${e.both ? '<->' : '->'} ${name(e.to)}: ${e.label}${e.dashed ? ' (in the background)' : ''}`);
  return [`Diagram: ${d.alt}`, '', ...arrows].join('\n');
}

function byName(d: Diagram, id: string): DiagramNode | undefined {
  return d.nodes.find((n) => n.id === id);
}
