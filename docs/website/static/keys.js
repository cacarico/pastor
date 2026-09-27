// Keys, like the TUI the site looks like: 1-4 switch tabs, j/k walk the page
// list, enter opens the focused page. Every target is a plain link too, so
// the site works the same without this file.

// A link marked data-confirm leaves the site, so it asks first, the way a
// terminal asks before doing something you may not have meant: y or enter
// goes, n or esc stays.
const dialog = document.getElementById('confirm');
let pending = null;

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
  // A new tab, so the docs stay where you left them. If the browser blocks
  // the tab, fall back to going there in this one.
  // window.open with "noopener" always returns null, so cut the opener by
  // hand instead: the new tab must not be able to reach back into this one.
  if (dialog.returnValue === 'yes' && pending) {
    const tab = window.open(pending, '_blank');
    if (tab) tab.opener = null;
    else location.href = pending;
  }
  pending = null;
});

document.addEventListener('keydown', (e) => {
  if (e.ctrlKey || e.metaKey || e.altKey || e.target.closest?.('input, textarea')) return;
  if (dialog?.open) {
    if (e.key === 'y') dialog.close('yes');
    if (e.key === 'n') dialog.close('no');
    return;  // esc and enter are the dialog's own
  }
  const tab = document.querySelector(`.tabs [data-key="${e.key}"]`);
  if (tab) { tab.click(); return; }
  if (e.key !== 'j' && e.key !== 'k') return;
  const items = [...document.querySelectorAll('.list a')];
  if (!items.length) return;
  let i = items.indexOf(document.activeElement);
  if (i < 0) i = Math.max(0, items.findIndex((a) => a.classList.contains('sel')));
  else i += e.key === 'j' ? 1 : -1;
  items[(i + items.length) % items.length].focus();
});
