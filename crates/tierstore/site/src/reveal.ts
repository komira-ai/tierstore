// Scroll reveal, as on komira.ai: a block marked `k-rv` that starts below the fold fades up when it scrolls into view.
// Progressive enhancement: without this script, or with prefers-reduced-motion, or without IntersectionObserver, no
// block is ever hidden (the hiding rule only applies under `.js` and `.is-pre`, which are added here).

/** Whether a block at `top` (its distance from the top of the viewport) starts below the first screen. */
export function startsBelowFold(top: number, viewportHeight: number): boolean {
  return top >= viewportHeight * 0.92;
}

export function setupReveal(doc: Document, win: Window): void {
  if (!('IntersectionObserver' in win)) return;
  if (win.matchMedia('(prefers-reduced-motion: reduce)').matches) return;
  doc.documentElement.classList.add('js');
  const io = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        if (e.isIntersecting) {
          e.target.classList.remove('is-pre');
          io.unobserve(e.target);
        }
      }
    },
    { rootMargin: '0px 0px -8% 0px' },
  );
  for (const el of doc.querySelectorAll('.k-rv')) {
    if (!startsBelowFold(el.getBoundingClientRect().top, win.innerHeight)) continue;
    el.classList.add('is-pre');
    io.observe(el);
  }
}

setupReveal(document, window);
