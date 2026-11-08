// The docs menu on a phone: the list of the guide's pages is a disclosure that starts closed there. In the markup it is
// open (what a wide screen and a browser with no script show); this closes it below the width where the list sits beside
// the page, and opens it again if the window grows past that width.

const WIDE = '(min-width: 60rem)';

const menu = document.querySelector<HTMLDetailsElement>('details.docs-menu');
if (menu !== null) {
  const wide = window.matchMedia(WIDE);
  const sync = (): void => {
    menu.open = wide.matches;
  };
  sync();
  wide.addEventListener('change', sync);
}
export {};
