// Keys, like the TUI the site looks like, with Vimium's scrolling:
//   1-4        switch tabs
//   j k        scroll down, up         d u   half a page down, up
//   h l        scroll left, right      gg G  top, bottom
// Every target is a plain link too, so the site works without this file.

const STEP = 60;  // pixels per j/k/h/l, Vimium's default
const dialog = document.getElementById('confirm');
let pending = null;

// A link marked data-confirm leaves the site, so it asks first, the way a
// terminal asks before doing something you may not have meant: y or enter
// goes, n or esc stays.
document.addEventListener('click', (e) => {
  const link = e.target.closest('a[data-confirm]');
  if (!link || !dialog || e.ctrlKey || e.metaKey || e.shiftKey) return;
  e.preventDefault();
  pending = link.href;
  document.getElementById('confirm-dest').textContent = link.host + link.pathname;
  dialog.returnValue = '';
  dialog.showModal();
});

dialog?.addEventListener('close', () => {
  // A new tab, so the docs stay where they were. window.open with "noopener"
  // always returns null, so the opener is cut by hand; if the browser blocks
  // the tab, go there in this one.
  if (dialog.returnValue === 'yes' && pending) {
    const tab = window.open(pending, '_blank');
    if (tab) tab.opener = null;
    else location.href = pending;
  }
  pending = null;
});

// What h and l move: the page if it is wider than the window, else the code
// block or table under the pointer, else the one nearest the middle of the
// screen. Blocks that fit need no scrolling and are skipped.
const wide = (el) => el.scrollWidth > el.clientWidth + 1;
function sideways() {
  const page = document.scrollingElement;
  if (wide(page)) return page;
  const blocks = [...document.querySelectorAll('pre, .doc table')].filter(wide);
  const hovered = blocks.find((el) => el.matches(':hover'));
  if (hovered) return hovered;
  const mid = innerHeight / 2;
  const onScreen = blocks
    .map((el) => ({ el, r: el.getBoundingClientRect() }))
    .filter(({ r }) => r.bottom > 0 && r.top < innerHeight);
  onScreen.sort((a, b) =>
    Math.abs((a.r.top + a.r.bottom) / 2 - mid) - Math.abs((b.r.top + b.r.bottom) / 2 - mid));
  return onScreen[0]?.el;
}

// Smooth for one press; instant while a key is held, so repeats do not queue
// up behind the animation. Reduced motion is always instant.
const still = matchMedia('(prefers-reduced-motion: reduce)');
const scroll = (el, left, top, repeat) =>
  el?.scrollBy({ left, top, behavior: repeat || still.matches ? 'instant' : 'smooth' });

let lastG = 0;
document.addEventListener('keydown', (e) => {
  if (e.ctrlKey || e.metaKey || e.altKey || e.target.closest?.('input, textarea')) return;
  if (dialog?.open) {
    if (e.key === 'y') dialog.close('yes');
    if (e.key === 'n') dialog.close('no');
    return;  // esc and enter are the dialog's own
  }
  const tab = document.querySelector(`.tabs [data-key="${e.key}"]`);
  if (tab) { tab.click(); return; }

  const page = document.scrollingElement;
  switch (e.key) {
    case 'j': scroll(page, 0, STEP, e.repeat); break;
    case 'k': scroll(page, 0, -STEP, e.repeat); break;
    case 'd': scroll(page, 0, innerHeight / 2, e.repeat); break;
    case 'u': scroll(page, 0, -innerHeight / 2, e.repeat); break;
    case 'h': scroll(sideways(), -STEP, 0, e.repeat); break;
    case 'l': scroll(sideways(), STEP, 0, e.repeat); break;
    case 'G': scroll(page, 0, page.scrollHeight, false); break;
    case 'g':
      // gg: two presses within 800ms, as in vim.
      if (Date.now() - lastG < 800) { page.scrollTo({ top: 0, behavior: still.matches ? 'instant' : 'smooth' }); lastG = 0; }
      else lastG = Date.now();
      break;
    default: return;
  }
  e.preventDefault();
});
