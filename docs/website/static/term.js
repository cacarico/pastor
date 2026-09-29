// Replays the homepage's transcript as a live session: commands typed a
// key at a time, output printed at once, one act on screen at a time, then
// round again. The transcript is plain text in the page, so without this
// file, or with reduced motion, it simply reads as it is.
// Loaded as a module, so its names never meet keys.js's globals.

const term = document.querySelector('[data-term]');
const READ_PAUSE = 6000; // ms a finished act stays on screen
const PAUSE_ICON = '<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="4" y="3" width="3" height="10" rx="1"/><rect x="9" y="3" width="3" height="10" rx="1"/></svg>';
const PLAY_ICON = '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M5 3v10l8-5z"/></svg>';
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

  // Controls on the pane's top border: a dot per act (the one playing is
  // filled; click one to jump to it), pause, and replay from the start.
  const ctl = term.parentElement.querySelector('.term-ctl');
  const actBtns = [...(ctl?.querySelectorAll('[data-term-act]') ?? [])];
  const pauseBtn = ctl?.querySelector('[data-term-pause]');
  const replayBtn = ctl?.querySelector('[data-term-replay]');
  if (ctl) ctl.hidden = false;
  // The pane's title names the act on screen, so a switch reads as one.
  const title = term.parentElement.querySelector('[data-term-title]');
  const mark = (i) => {
    actBtns.forEach((b, j) => {
      b.classList.toggle('on', i === j);
      b.setAttribute('aria-pressed', String(i === j));
    });
    if (title && actBtns[i]) title.textContent = `· ${actBtns[i].title}`;
  };

  // The window keeps its newest line in view, as a terminal scrolls.
  const follow = () => { term.scrollTop = term.scrollHeight; };

  let paused = false;
  let run = 0; // bumped by replay or a dot: an older loop sees it and stops

  // On screen, read from the page each time rather than kept in a flag, so a
  // missed observer callback can never leave the clock stopped for good.
  const onScreen = () => {
    const r = term.getBoundingClientRect();
    return r.bottom > 0 && r.top < innerHeight;
  };

  // Waits `ms` of playing time: the clock stops while paused, scrolled away
  // or in a hidden tab. A stopped wait parks in `parked` with no timer
  // running, and `wake` restarts it when playing resumes.
  const active = () => !paused && !document.hidden && onScreen();
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

  async function loop(mine, start = 0) {
    for (let i = start; ; i = (i + 1) % acts.length) {
      mark(i);
      term.textContent = '';
      for (const line of acts[i]) {
        if (!(await put(line, mine))) return;
      }
      // A full screen stays up long enough to read before it clears.
      if (!(await wait(READ_PAUSE, mine))) return;
    }
  }

  // One line: an input line is typed after its prompt, anything else
  // printed whole.
  async function put(line, mine) {
    const el = line.cloneNode(true);
    if (line.dataset.kind !== 'in') {
      // An assistant block waits a moment, as if thinking, and starts after a
      // blank line, as Claude Code draws it.
      const pause = Number(line.dataset.pause) || (line.dataset.kind === 'lbl' ? 300 : 350);
      if (!(await wait(pause, mine))) return false;
      if (line.hasAttribute('data-gap')) term.append('\n');
      term.append(el, '\n');
      follow();
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
    follow();
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

  const setPaused = (p) => {
    paused = p;
    if (!pauseBtn) return;
    pauseBtn.innerHTML = p ? PLAY_ICON : PAUSE_ICON;
    pauseBtn.setAttribute('aria-label', p ? 'play' : 'pause');
    pauseBtn.title = p ? 'play' : 'pause';
  };
  // Start act `i` over: the old loop's parked or running wait sees the new
  // run and ends.
  const restart = (i) => {
    setPaused(false);
    run++;
    wake();
    loop(run, i);
  };
  pauseBtn?.addEventListener('click', () => { setPaused(!paused); wake(); });
  replayBtn?.addEventListener('click', () => restart(0));
  actBtns.forEach((b, i) => b.addEventListener('click', () => restart(i)));

  new IntersectionObserver(wake).observe(term);
  addEventListener('scroll', wake, { passive: true });
  addEventListener('resize', wake);
  document.addEventListener('visibilitychange', wake);
  loop(run);
}
