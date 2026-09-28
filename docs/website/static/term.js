// Replays the homepage's transcript as a live session: commands typed a
// key at a time, output printed at once, one act on screen at a time, then
// round again. The transcript is plain text in the page, so without this
// file, or with reduced motion, it simply reads as it is.
// Loaded as a module, so its names never meet keys.js's globals.

const term = document.querySelector('[data-term]');
const READ_PAUSE = 6000; // ms a finished act stays on screen
const still = matchMedia('(prefers-reduced-motion: reduce)');

if (term && !still.matches) play(term);

function play(term) {
  // Acts, each a list of lines; a label line starts a new act.
  const acts = [];
  for (const line of term.querySelectorAll('.l')) {
    if (line.dataset.kind === 'lbl' || !acts.length) acts.push([]);
    acts[acts.length - 1].push(line.cloneNode(true));
  }

  // Screen readers get the whole transcript once, not a screen being typed.
  const copy = term.cloneNode(true);
  copy.removeAttribute('data-term');
  copy.className = 'sr-only';
  term.after(copy);
  term.setAttribute('aria-hidden', 'true');

  // As tall as the tallest act, so the page does not jump as lines arrive.
  let tallest = 0;
  for (const act of acts) {
    show(act);
    tallest = Math.max(tallest, term.offsetHeight);
  }
  term.style.height = `${tallest}px`;

  const ctl = term.parentElement.querySelector('.ctl');
  const pauseBtn = ctl?.querySelector('[data-term-pause]');
  const replayBtn = ctl?.querySelector('[data-term-replay]');
  if (ctl) ctl.hidden = false;

  let paused = false;
  let seen = true;
  let run = 0; // bumped by replay: an older loop sees it and stops

  // Waits `ms` of playing time: the clock stops while paused, scrolled away
  // or in a hidden tab. A stopped wait parks in `parked` with no timer
  // running, and `wake` restarts it when playing resumes.
  const active = () => !paused && seen && !document.hidden;
  let parked = [];
  const wake = () => {
    if (!active()) return;
    const ready = parked;
    parked = [];
    for (const go of ready) go();
  };
  const wait = (ms, mine) => new Promise((done) => {
    let left = ms;
    const tick = () => {
      if (mine !== run) return done(false);
      if (left <= 0) return done(true);
      if (!active()) return parked.push(tick);
      const step = Math.min(left, 50);
      setTimeout(() => {
        if (active()) left -= step;
        tick();
      }, step);
    };
    tick();
  });

  async function loop(mine) {
    for (;;) {
      for (const act of acts) {
        term.textContent = '';
        for (const line of act) {
          if (!(await put(line, mine))) return;
        }
        // A full screen stays up long enough to read before it clears.
        if (!(await wait(READ_PAUSE, mine))) return;
      }
    }
  }

  // One line: an input line is typed after its prompt, anything else
  // printed whole.
  async function put(line, mine) {
    const el = line.cloneNode(true);
    if (line.dataset.kind !== 'in') {
      if (!(await wait(line.dataset.kind === 'lbl' ? 300 : 350, mine))) return false;
      term.append(el, '\n');
      return wait(line.dataset.kind === 'lbl' ? 900 : 250, mine);
    }
    const prompt = el.querySelector('.p');
    const text = el.textContent.slice(prompt ? prompt.textContent.length : 0);
    el.textContent = '';
    if (prompt) el.append(prompt);
    const typed = document.createTextNode('');
    const cursor = document.createElement('span');
    cursor.className = 'cursor';
    cursor.textContent = ' ';
    el.append(typed, cursor);
    term.append(el);
    if (!(await wait(500, mine))) return false;
    for (const ch of text) {
      typed.data += ch;
      if (!(await wait(18 + Math.random() * 40, mine))) return false;
    }
    if (!(await wait(400, mine))) return false;
    cursor.remove();
    term.append('\n');
    return true;
  }

  function show(act) {
    term.textContent = '';
    for (const line of act) term.append(line.cloneNode(true), '\n');
  }

  pauseBtn?.addEventListener('click', () => {
    paused = !paused;
    pauseBtn.textContent = paused ? 'play' : 'pause';
    wake();
  });
  replayBtn?.addEventListener('click', () => {
    paused = false;
    if (pauseBtn) pauseBtn.textContent = 'pause';
    run++;
    wake(); // the old loop's parked wait sees the new run and ends
    loop(run);
  });

  new IntersectionObserver(([entry]) => {
    seen = entry.isIntersecting;
    wake();
  }).observe(term);
  document.addEventListener('visibilitychange', wake);
  loop(run);
}
