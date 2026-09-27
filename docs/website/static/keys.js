// Keys, like the TUI the site looks like: 1-4 switch tabs, j/k walk the page
// list, enter opens the focused page. Every target is a plain link too, so
// the site works the same without this file.
document.addEventListener('keydown', (e) => {
  if (e.ctrlKey || e.metaKey || e.altKey || e.target.matches('input, textarea')) return;
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
