// The slime companion: artwork (createSlime), customization model, and the
// engine (Companion) that lets it live on the chat stage — it rests in a lane
// reserved above the composer so it can never cover messages, runs back and
// forth while SUI works, follows the cursor, and can be picked up and poked.
// Presentation only: nothing here reaches the agent or any persisted state
// other than the user's own appearance preferences.
(() => {
  'use strict';

  const COLORS = {
    azure: { label: 'Azure', light: '#b2d6ff', body: '#4280f8', deep: '#14308e', rim: '#d0e6ff' },
    mint: { label: 'Mint', light: '#b9f6d9', body: '#34d399', deep: '#0b6b4d', rim: '#d8fbe9' },
    grape: { label: 'Grape', light: '#dcc8ff', body: '#8b5cf6', deep: '#3b1d8f', rim: '#ebdfff' },
    rose: { label: 'Rose', light: '#ffd0dc', body: '#fb7185', deep: '#8f1d3f', rim: '#ffe3ea' },
    amber: { label: 'Amber', light: '#ffe9a8', body: '#f5b82e', deep: '#8a5a05', rim: '#fff3cf' },
    ink: { label: 'Ink', light: '#9aa7bd', body: '#3b4558', deep: '#0f1521', rim: '#c5cedd' }
  };
  const ACCESSORIES = { none: 'None', crown: 'Crown', sprout: 'Sprout', bow: 'Bow', shades: 'Shades' };
  // width in px, and the lane height that leaves room for a hop and the mood props
  const SIZES = { s: { w: 58, lane: 80 }, m: { w: 72, lane: 94 }, l: { w: 92, lane: 116 } };
  const DEFAULTS = { on: true, color: 'azure', accessory: 'none', size: 'm', name: 'Sui', follow: true };
  const PHRASES = ['oozing through your repo…', 'squishing bugs…', 'jiggling the code…', 'gelling the details…', 'wobbling through the files…', 'bubbling up ideas…'];
  const POKES = ['boing!', 'squish!', 'hehe', 'whoa!', 'blub blub'];

  const SLEEP_MS = 45000;
  const HOP_MS = 440;
  const GRAVITY = 2400;

  function sanitize(value) {
    const p = value && typeof value === 'object' ? value : {};
    const name = typeof p.name === 'string' ? p.name.replace(/[\u0000-\u001f\u007f]/g, '').trim().slice(0, 20) : DEFAULTS.name;
    return {
      on: p.on !== false,
      color: Object.hasOwn(COLORS, p.color) ? p.color : DEFAULTS.color,
      accessory: Object.hasOwn(ACCESSORIES, p.accessory) ? p.accessory : DEFAULTS.accessory,
      size: Object.hasOwn(SIZES, p.size) ? p.size : DEFAULTS.size,
      name: name || DEFAULTS.name,
      follow: p.follow !== false
    };
  }

  // ── motion math (pure; mirrors the terminal slime's hop) ──────────────

  const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));
  function smooth(e0, e1, x) {
    const t = clamp((x - e0) / (e1 - e0), 0, 1);
    return t * t * (3 - 2 * t);
  }
  // Crouch (0–.14), leap (.14–.8), land (.8–1). lift is 0..1 of the hop peak.
  function hopPose(ph) {
    const a = clamp((ph - 0.14) / 0.66, 0, 1);
    const q = ph < 0.14 ? 1 - smooth(0, 0.14, ph) : smooth(0.8, 0.97, ph);
    const air = Math.sin(Math.PI * a);
    return { lift: 4 * a * (1 - a), sx: 1 + 0.16 * q - 0.07 * air, sy: 1 - 0.2 * q + 0.12 * air };
  }
  // Horizontal progress through a hop: the slime only travels while airborne.
  function hopTravel(ph) { return smooth(0.14, 0.8, ph); }

  // ── artwork ───────────────────────────────────────────────────────────

  let uid = 0;
  const BODY = 'M9 66C9 40 26 14 51 14C76 14 91 40 91 66C91 75 78 79 50 79C22 79 9 75 9 66Z';
  function template(id) {
    return `
<div class="slime-shadow"></div>
<div class="slime-body">
<svg class="slime-svg" viewBox="0 0 100 84" focusable="false">
  <defs><linearGradient id="sg${id}" gradientUnits="userSpaceOnUse" x1="0" y1="12" x2="0" y2="80"><stop class="sg-a" offset="0"/><stop class="sg-b" offset=".46"/><stop class="sg-c" offset="1"/></linearGradient></defs>
  <g class="slime-breathe">
    <path class="slime-body-shape" fill="url(#sg${id})" d="M55 16C53 8 59 3 64 8C62 10.5 61 13 61.5 17Z"/>
    <path class="slime-body-shape" fill="url(#sg${id})" d="${BODY}"/>
    <path class="slime-rim-light" d="M17 53C18 35 30 21 46 17.5"/>
    <g class="slime-bubbles"><circle class="slime-bubble" cx="30" cy="68" r="3.2"/><circle class="slime-bubble" cx="69" cy="62" r="2.4"/><circle class="slime-bubble" cx="57" cy="71" r="1.8"/></g>
    <ellipse class="slime-gloss" cx="33" cy="31" rx="10" ry="5.2" transform="rotate(-32 33 31)"/>
    <circle class="slime-gloss-dot" cx="47" cy="23.5" r="2.4"/>
    <ellipse class="cheek" cx="24.5" cy="59.5" rx="6.5" ry="3.6"/><ellipse class="cheek" cx="75.5" cy="59.5" rx="6.5" ry="3.6"/>
    <g class="slime-look">
      <g class="eyes eyes-open"><g class="slime-eye l"><ellipse class="eye" cx="36" cy="46" rx="5" ry="6.6"/><circle class="glint" cx="34.2" cy="43" r="1.9"/><circle class="glint" cx="37.6" cy="49.2" r=".9" opacity=".6"/></g><g class="slime-eye r"><ellipse class="eye" cx="64" cy="46" rx="5" ry="6.6"/><circle class="glint" cx="62.2" cy="43" r="1.9"/><circle class="glint" cx="65.6" cy="49.2" r=".9" opacity=".6"/></g></g>
      <g class="eyes eyes-wide"><ellipse class="eye" cx="36" cy="46" rx="6.2" ry="7.8"/><circle class="glint" cx="33.8" cy="42.6" r="2.3"/><ellipse class="eye" cx="64" cy="46" rx="6.2" ry="7.8"/><circle class="glint" cx="61.8" cy="42.6" r="2.3"/></g>
      <g class="eyes eyes-happy"><path class="stroke-ink" stroke-width="3.2" d="M30 49Q36 40.5 42 49"/><path class="stroke-ink" stroke-width="3.2" d="M58 49Q64 40.5 70 49"/></g>
      <g class="eyes eyes-closed"><path class="stroke-ink" stroke-width="2.8" d="M30.5 46Q36 51.5 41.5 46"/><path class="stroke-ink" stroke-width="2.8" d="M58.5 46Q64 51.5 69.5 46"/></g>
      <g class="brows"><path class="stroke-ink" stroke-width="2.4" d="M29 40.5L42 36.5"/><path class="stroke-ink" stroke-width="2.4" d="M58 36.5L71 40.5"/></g>
    </g>
    <g class="mouths">
      <path class="mouth m-idle stroke-ink" stroke-width="2.6" d="M44 59Q50 64.5 56 59"/>
      <g class="mouth m-happy"><path class="fill-mouth" d="M41.5 57Q50 76 58.5 57Z"/><ellipse class="fill-tongue" cx="50" cy="66.5" rx="4.6" ry="2.6"/></g>
      <path class="mouth m-oops stroke-ink" stroke-width="2.6" d="M43 65.5Q50 58.5 57 65.5"/>
      <ellipse class="mouth m-think eye" cx="50" cy="61" rx="3" ry="3.6" opacity=".85"/>
      <path class="mouth m-ask stroke-ink" stroke-width="2.4" d="M40.5 61.5q2.5-3.5 5 0t5 0t5 0t5 0"/>
      <ellipse class="mouth m-sleep eye" cx="50" cy="61" rx="2.2" ry="2.8" opacity=".8"/>
      <ellipse class="mouth m-poke fill-mouth" cx="50" cy="62" rx="4.4" ry="5.4"/>
    </g>
    <g class="acc acc-crown" transform="rotate(-8 50 18)"><path d="M37 19L39 6.5L45 13.5L50 4L55 13.5L61 6.5L63 19Z" fill="#ffcf4a" stroke="#b9801a" stroke-width="1.5" stroke-linejoin="round"/><circle cx="50" cy="12" r="1.7" fill="#ff5d7a"/><circle cx="41" cy="14" r="1.2" fill="#5db8ff"/><circle cx="59" cy="14" r="1.2" fill="#5de0a0"/></g>
    <g class="acc acc-sprout"><path d="M52 16Q52 9 55 4" fill="none" stroke="#2a9d62" stroke-width="2.4" stroke-linecap="round"/><path d="M55 6.5C63 .5 71 4.5 70 10.5C63 12.5 57 10.5 55 6.5Z" fill="#46d28b"/><path d="M53.5 9C46 3 38 6 38.5 12C45.5 14 51.5 13 53.5 9Z" fill="#2fb978"/></g>
    <g class="acc acc-bow"><path d="M31 23L18 15.5V30.5Z" fill="#ff6fa3"/><path d="M31 23L44 15.5V30.5Z" fill="#ff6fa3"/><circle cx="31" cy="23" r="3.6" fill="#ff3f86"/></g>
    <g class="acc acc-shades"><path d="M25 41.5H47V50Q47 55 41.5 55H30.5Q25 55 25 50Z" fill="#0b1020" opacity=".94"/><path d="M53 41.5H75V50Q75 55 69.5 55H58.5Q53 55 53 50Z" fill="#0b1020" opacity=".94"/><path d="M47 44.5H53" stroke="#0b1020" stroke-width="2.2"/><path d="M28.5 44.5L33 44.5" stroke="#fff" stroke-opacity=".4" stroke-width="1.6" stroke-linecap="round"/></g>
  </g>
  <g class="prop prop-q"><circle class="q-bg" cx="80" cy="14" r="8.5"/><text class="q-mark" x="80" y="18.5">?</text></g>
  <g class="prop prop-z"><text x="70" y="22">z</text><text x="78" y="14">z</text><text x="86" y="6">z</text></g>
  <g class="prop prop-think"><circle cx="76" cy="14" r="2.2"/><circle cx="84" cy="8" r="2.8"/><circle cx="93" cy="1" r="3.4"/></g>
  <path class="prop prop-sweat" d="M78 27C75 31.5 74 33.5 77 35.5C80 33.5 81 31.5 78 27Z"/>
  <g class="prop prop-hearts"><path d="M82 18c-3-4-8-1-5 3l5 4 5-4c3-4-2-7-5-3z"/><path d="M16 22c-2-3-6-1-4 2l4 3 4-3c2-3-2-5-4-2z"/><path d="M60 8c-2-3-6-1-4 2l4 3 4-3c2-3-2-5-4-2z"/></g>
</svg>
</div>`;
  }

  // The character on its own: set the mood and where it looks; nothing moves it.
  function createSlime({ demo = false } = {}) {
    const el = document.createElement('div');
    el.className = demo ? 'slime demo' : 'slime';
    el.dataset.mood = 'idle';
    el.dataset.acc = 'none';
    el.innerHTML = template(++uid);
    return {
      el,
      setMood(mood) { el.dataset.mood = mood; },
      setFlag(name, on) { if (on) el.dataset[name] = '1'; else delete el.dataset[name]; },
      setLook(x, y) { el.style.setProperty('--ex', (x * 2.8).toFixed(2)); el.style.setProperty('--ey', (y * 2.2).toFixed(2)); },
      setPose(lift, sx, sy) { el.style.setProperty('--lift', lift.toFixed(2)); el.style.setProperty('--sx', sx.toFixed(3)); el.style.setProperty('--sy', sy.toFixed(3)); },
      setAppearance(prefs) {
        const c = COLORS[prefs.color];
        el.style.setProperty('--pet-light', c.light);
        el.style.setProperty('--pet-body', c.body);
        el.style.setProperty('--pet-deep', c.deep);
        el.style.setProperty('--pet-rim', c.rim);
        el.style.setProperty('--pet-aura', c.body + '61');
        el.dataset.acc = prefs.accessory;
      }
    };
  }

  // ── the engine ────────────────────────────────────────────────────────

  class Companion {
    // layer: positioned overlay over the stage; lane: reserved strip above the
    // composer; hero: slot on the empty-state screen; stage: element with data-stage.
    constructor({ layer, lane, hero, stage, prefs, motion }) {
      Object.assign(this, { layer, lane, hero, stage });
      this.prefs = sanitize(prefs);
      this.motion = motion || 'full';
      this.slime = createSlime();
      this.root = document.createElement('div');
      this.root.className = 'pet';
      this.root.append(this.slime.el);
      this.bubble = document.createElement('div');
      this.bubble.className = 'pet-bubble';
      layer.append(this.root, this.bubble);

      this.flags = { busy: false, asking: false };
      this.temp = null;
      this.mood = 'idle';
      this.cx = 0; this.ground = 0; this.h = 0; this.vh = 0; this.scale = 1;
      this.mode = null;
      this.look = { x: 0, y: 0 };
      this.pointer = { x: 0, y: 0, at: 0, inside: false };
      this.hop = null; this.dir = 1; this.celebrate = 0;
      this.drag = null; this.dragPos = null;
      this.t = performance.now();
      this.lastActive = this.t; this.lastHopEnd = 0; this.nextWander = this.t + 9000;
      this.pokeAt = -1e9; this.tapAt = -1e9; this.landAt = -1e9;
      this.say = { until: 0, text: '', phraseAt: 0, phrase: 0 };
      this.raf = 0; this.timer = 0; this.placed = false; this.bubbleOn = false;
      this.typingLook = 0;

      this.configure(this.prefs);
      this.bind();
      const ro = new ResizeObserver(() => this.kick());
      ro.observe(layer);
      ro.observe(lane);
      this.kick();
    }

    configure(prefs) {
      this.prefs = sanitize(prefs);
      const size = SIZES[this.prefs.size];
      this.baseW = size.w;
      this.baseH = size.w * 0.84;
      this.root.style.setProperty('--pet-w', size.w + 'px');
      this.root.hidden = !this.prefs.on;
      if (!this.prefs.on) { this.say.until = 0; this.bubbleOn = false; this.bubble.classList.remove('show'); }
      this.slime.setAppearance(this.prefs);
      document.documentElement.style.setProperty('--lane-h', size.lane + 'px');
      this.kick();
    }

    setMotion(motion) {
      this.motion = motion;
      if (motion !== 'full') { this.hop = null; this.celebrate = 0; this.h = 0; this.vh = 0; }
      this.kick();
    }

    // ── app hooks ───────────────────────────────────────────────────────
    setBusy(on) { this.flags.busy = on; this.activity(); this.kick(); }
    refresh() { this.kick(); }
    setAsking(on) { this.flags.asking = on; this.activity(); this.kick(); }
    cheer() { this.temp = { mood: 'happy', until: performance.now() + 6000 }; this.celebrate = 3; this.activity(); this.kick(); }
    fret() { this.temp = { mood: 'oops', until: performance.now() + 12000 }; this.activity(); this.kick(); }
    typing() { this.tapAt = performance.now(); this.typingLook = this.tapAt + 1500; this.activity(); this.kick(); }
    activity() { this.lastActive = performance.now(); }
    poke() {
      const now = performance.now();
      this.pokeAt = now;
      this.slime.setFlag('poke', true);
      setTimeout(() => this.slime.setFlag('poke', false), 700);
      const line = POKES[Math.floor(Math.random() * POKES.length)];
      this.speak(Math.random() < 0.2 ? `hi, I'm ${this.prefs.name}!` : line, 1800);
      this.activity();
      this.kick();
    }
    speak(text, ms, persist = false) {
      this.say.text = text;
      this.say.until = persist ? Infinity : performance.now() + ms;
      this.bubble.textContent = text;
    }

    // ── input ───────────────────────────────────────────────────────────
    bind() {
      const wake = () => { this.activity(); this.kick(); };
      addEventListener('pointermove', e => {
        this.pointer = { x: e.clientX, y: e.clientY, at: performance.now(), inside: true };
        if (this.mood === 'sleep') wake();
        else this.activity();
        if (this.motion !== 'off' && !this.timer && !this.raf) this.kick();
      }, { passive: true });
      document.documentElement.addEventListener('mouseleave', () => { this.pointer.inside = false; });
      addEventListener('keydown', wake, true);
      addEventListener('pointerdown', wake, true);
      document.addEventListener('visibilitychange', () => { if (!document.hidden) this.kick(); });

      this.root.addEventListener('pointerdown', e => {
        if (e.button !== 0) return;
        this.root.setPointerCapture(e.pointerId);
        this.drag = { id: e.pointerId, x: e.clientX, y: e.clientY, moved: false };
      });
      this.root.addEventListener('pointermove', e => {
        const d = this.drag;
        if (!d || d.id !== e.pointerId) return;
        if (!d.moved && Math.hypot(e.clientX - d.x, e.clientY - d.y) > 5) {
          d.moved = true;
          this.hop = null;
          this.root.classList.add('dragging');
          this.slime.setFlag('drag', true);
        }
        if (d.moved) {
          const L = this.layer.getBoundingClientRect();
          this.dragPos = { x: e.clientX - L.left, y: e.clientY - L.top };
          this.kick();
        }
      });
      const release = e => {
        const d = this.drag;
        if (!d || d.id !== e.pointerId) return;
        this.drag = null;
        this.dragPos = null;
        this.root.classList.remove('dragging');
        this.slime.setFlag('drag', false);
        if (!d.moved) this.poke();
        else { this.vh = 0; this.activity(); }
        this.kick();
      };
      this.root.addEventListener('pointerup', release);
      this.root.addEventListener('pointercancel', release);
      this.root.addEventListener('dblclick', () => {
        if (this.motion === 'full') this.startHop(this.cx, 1.7, 640);
        this.speak('wheee!', 1400);
        this.kick();
      });
    }

    // ── loop ────────────────────────────────────────────────────────────
    kick() {
      if (this.prefs.on === false || document.hidden) return;
      clearTimeout(this.timer);
      this.timer = 0;
      if (!this.raf) this.raf = requestAnimationFrame(t => { this.raf = 0; this.step(t); });
    }

    metrics() {
      const L = this.layer.getBoundingClientRect();
      const hero = this.stage.dataset.stage === 'hero';
      const laneR = this.lane.getBoundingClientRect();
      const heroR = this.hero.getBoundingClientRect();
      const mode = hero && heroR.height > 0 ? 'hero' : 'lane';
      let scale = 1, cxMin, cxMax, ground;
      if (mode === 'hero') {
        scale = clamp((heroR.height * 0.92) / this.baseH, 1, 2.3);
        cxMin = cxMax = heroR.left - L.left + heroR.width / 2;
        ground = heroR.bottom - L.top - 4;
      } else {
        const half = (this.baseW * scale) / 2;
        cxMin = laneR.left - L.left + half + 14;
        cxMax = laneR.right - L.left - half - 14;
        if (cxMax < cxMin) cxMin = cxMax = (laneR.left + laneR.right) / 2 - L.left;
        ground = laneR.bottom - L.top - 8;
      }
      return { L, mode, scale, cxMin, cxMax, ground, laneH: laneR.height };
    }

    currentMood(now) {
      if (this.flags.asking) return 'ask';
      if (this.flags.busy) return 'think';
      if (this.temp && now < this.temp.until) return this.temp.mood;
      if (this.motion !== 'off' && now - this.lastActive > SLEEP_MS) return 'sleep';
      return 'idle';
    }

    onMood(prev, next, now) {
      this.slime.setMood(next);
      if (prev === 'ask') this.say.until = 0;
      if (next === 'ask') this.speak('need your OK', 0, true);
      else if (next === 'think') { this.say.phraseAt = now + 1600; this.say.phrase = Math.floor(Math.random() * PHRASES.length); }
      else if (next === 'happy') this.speak('all done!', 2600);
      else if (next === 'oops') this.speak('something went wrong', 3400);
      else if (prev === 'sleep') this.speak('mm?', 1400);
    }

    startHop(to, peak, dur) {
      this.hop = { t0: performance.now(), from: this.cx, to, peak, dur };
    }

    step(now) {
      const dt = clamp(now - this.t, 0, 48);
      this.t = now;
      if (this.prefs.on === false || document.hidden) return;
      const M = this.metrics();
      const mood = this.currentMood(now);
      if (mood !== this.mood) { const prev = this.mood; this.mood = mood; this.onMood(prev, mood, now); }
      const full = this.motion === 'full';
      const moving = this.motion !== 'off';

      // anchors and mode switches (empty-state hero ⇄ lane)
      if (!this.placed) {
        this.placed = true;
        this.mode = M.mode;
        this.cx = M.mode === 'hero' ? M.cxMin : clamp(M.cxMax, M.cxMin, M.cxMax);
        this.ground = M.ground;
        this.scale = M.scale;
        if (full && M.mode === 'hero') { this.h = 360; this.vh = 0; }
      } else if (M.mode !== this.mode) {
        this.mode = M.mode;
        this.ground = M.ground;
        // Stage changes must stay in their reserved regions. A falling
        // transition from the hero to the lane would cross chat messages.
        this.h = 0;
        this.vh = 0;
        this.scale = M.scale;
        this.hop = null;
      } else {
        // The hero slot moves when welcome content changes. Keep the slime in
        // its reserved slot immediately so it never slides across the title.
        if (M.mode === 'hero') this.ground = M.ground;
        else this.ground += (M.ground - this.ground) * clamp(dt / 90, 0, 1);
      }
      this.scale += (M.scale - this.scale) * clamp(dt / (moving ? 110 : 1), 0, 1);

      let sx = 1, sy = 1, lift = 0;
      if (this.dragPos) {
        const half = (this.baseH * this.scale) / 2;
        this.cx = this.dragPos.x;
        this.h = this.ground - (this.dragPos.y + half);
        this.vh = 0;
        sy = 1.14; sx = 0.92;
      } else {
        if (this.mode === 'hero') this.cx += (M.cxMin - this.cx) * clamp(dt / 120, 0, 1);
        else if (!this.hop) this.cx = clamp(this.cx, M.cxMin, M.cxMax);

        // gravity (drops, falls after a drag, mode switches); a slime let go
        // below its ground line hops back up onto it
        if (full && this.h < -1 && this.vh === 0) this.vh = Math.sqrt(2 * GRAVITY * (30 - this.h));
        if (full && (this.h > 0.1 || this.vh > 0 || this.h < 0)) {
          this.vh -= GRAVITY * dt / 1000;
          this.h += this.vh * dt / 1000;
          if (this.h <= 0 && this.vh <= 0) {
            this.h = 0;
            if (this.vh < -240) { this.landAt = now; this.puff(M); }
            this.vh = 0;
          }
        } else if (!full) { this.h = 0; this.vh = 0; }

        // behaviours that start hops
        if (full && !this.hop && this.h === 0) this.plan(M, mood, now);

        if (this.hop) {
          const k = (now - this.hop.t0) / this.hop.dur;
          if (k >= 1) {
            this.cx = clamp(this.hop.to, M.cxMin, M.cxMax);
            this.hop = null;
            this.lastHopEnd = now;
            this.landAt = now;
            if (mood === 'think') this.puff(M);
            if (mood === 'happy') this.sparkle(M);
          } else {
            const p = hopPose(k);
            this.cx = clamp(this.hop.from + (this.hop.to - this.hop.from) * hopTravel(k), M.cxMin, M.cxMax);
            lift = p.lift * this.hop.peak * this.baseH * 0.34;
            sx = p.sx; sy = p.sy;
          }
        }
      }

      // squash from landings, pokes and keystrokes
      if (moving) {
        const land = (now - this.landAt) / 1000;
        if (land < 0.8) { const q = 0.22 * Math.exp(-land / 0.12) * Math.cos(land * Math.PI * 6.4); sy -= q; sx += 0.8 * q; }
        const poke = (now - this.pokeAt) / 1000;
        if (poke < 1.5) { const q = 0.3 * Math.exp(-poke / 0.28) * Math.cos(poke * Math.PI * 8.4); sy -= q; sx += 0.75 * q; }
        const tap = (now - this.tapAt) / 1000;
        if (tap < 0.5) { const q = 0.1 * Math.exp(-tap / 0.09) * Math.sin(tap * Math.PI * 14); sy -= q; sx += 0.7 * q; }
      }
      if (mood === 'oops') { sy *= 0.9; sx *= 1.05; }
      if (mood === 'sleep') { sy *= 0.93; sx *= 1.04; }

      // where the eyes look
      let lx = 0, ly = 0;
      const petX = M.L.left + this.cx, petY = M.L.top + this.ground - this.h - (this.baseH * this.scale) / 2;
      if (mood === 'think' && this.hop) { lx = this.hop.to > this.hop.from ? 1 : -1; ly = -0.1; }
      else if (now < this.typingLook) { ly = 0.85; lx = 0.15; }
      else if (this.pointer.inside && now - this.pointer.at < 10000 && mood !== 'sleep') {
        lx = clamp((this.pointer.x - petX) / 220, -1, 1);
        ly = clamp((this.pointer.y - petY) / 220, -1, 1);
      }
      this.look.x += (lx - this.look.x) * clamp(dt / 80, 0, 1);
      this.look.y += (ly - this.look.y) * clamp(dt / 80, 0, 1);

      // write
      const w = this.baseW, hgt = this.baseH;
      this.root.style.transform = `translate3d(${(this.cx - w / 2).toFixed(1)}px, ${(this.ground - hgt).toFixed(1)}px, 0) scale(${this.scale.toFixed(3)})`;
      this.slime.setPose(this.h / this.scale + lift, sx, sy);
      this.slime.setLook(this.look.x, this.look.y);
      this.placeBubble(M, now);

      // schedule: animate while anything moves, otherwise just poll gently
      const settling = Math.abs(M.scale - this.scale) > 0.003 || Math.abs(M.ground - this.ground) > 0.5 || (this.mode === 'hero' && Math.abs(M.cxMin - this.cx) > 0.5);
      const reacting = now - this.landAt < 800 || now - this.pokeAt < 1500 || now - this.tapAt < 500 || now < this.typingLook;
      const lookDrift = Math.abs(lx - this.look.x) > 0.02 || Math.abs(ly - this.look.y) > 0.02;
      const bubbleLive = this.say.until > now || this.bubbleOn;
      const busy = this.hop || this.h !== 0 || this.vh !== 0 || this.dragPos || settling || reacting || lookDrift || bubbleLive || (full && (mood === 'think' || mood === 'ask' || (mood === 'happy' && this.celebrate > 0)));
      if (busy && moving) this.raf = requestAnimationFrame(t => { this.raf = 0; this.step(t); });
      else if (moving) this.timer = setTimeout(() => { this.timer = 0; this.kick(); }, mood === 'sleep' ? 600 : 260);
    }

    plan(M, mood, now) {
      const span = M.cxMax - M.cxMin;
      if (this.mode === 'hero' || span < 8) return;
      const D = clamp(span * 0.16, 70, 130);
      if (mood === 'think') {
        let to = this.cx + this.dir * D;
        if (to > M.cxMax || to < M.cxMin) { this.dir *= -1; to = clamp(this.cx + this.dir * D, M.cxMin, M.cxMax); }
        this.startHop(to, 1, HOP_MS);
      } else if (mood === 'ask') {
        this.startHop(this.cx, 0.4, 560);
      } else if (mood === 'happy' && this.celebrate > 0) {
        this.celebrate -= 1;
        this.startHop(this.cx, 1.35, 540);
      } else if (mood === 'idle' && now - this.lastHopEnd > 500) {
        const seen = this.pointer.inside && now - this.pointer.at < 8000;
        if (this.prefs.follow && seen) {
          const want = clamp(this.pointer.x - M.L.left, M.cxMin, M.cxMax);
          const dx = want - this.cx;
          if (Math.abs(dx) > 150) this.startHop(this.cx + Math.sign(dx) * Math.min(Math.abs(dx) - 90, D), 1, HOP_MS);
        } else if (!seen && now > this.nextWander) {
          this.nextWander = now + 9000 + Math.random() * 9000;
          const want = M.cxMin + Math.random() * span;
          this.startHop(this.cx + Math.sign(want - this.cx) * Math.min(Math.abs(want - this.cx), D), 0.8, HOP_MS);
        }
      }
    }

    placeBubble(M, now) {
      const s = this.say;
      if (this.mood === 'think' && now >= s.phraseAt && s.until !== Infinity) {
        s.phraseAt = now + 4800;
        s.phrase = (s.phrase + 1) % PHRASES.length;
        this.speak(PHRASES[s.phrase], 3600);
      }
      const show = now < s.until;
      if (show !== this.bubbleOn) { this.bubbleOn = show; this.bubble.classList.toggle('show', show); }
      if (!show) return;
      const bw = this.bubble.offsetWidth, bh = this.bubble.offsetHeight;
      const pw = this.baseW * this.scale, ph = this.baseH * this.scale;
      const top = this.ground - this.h - ph;
      let x, y;
      if (this.mode === 'hero') { x = this.cx - bw / 2; y = top - bh - 6; }
      else {
        x = this.cx < M.L.width * 0.55 ? this.cx + pw / 2 + 8 : this.cx - pw / 2 - bw - 8;
        y = top + ph * 0.5 - bh / 2;
      }
      x = clamp(x, 6, Math.max(6, M.L.width - bw - 6));
      this.bubble.style.transform = `translate3d(${x.toFixed(1)}px, ${y.toFixed(1)}px, 0)`;
    }

    sparkle(M) {
      if (this.motion !== 'full') return;
      const cx = this.cx, y = this.ground - this.baseH * this.scale * 0.7 - this.h;
      for (let i = 0; i < 9; i++) {
        const s = document.createElement('i');
        s.className = 'pet-spark';
        const a = (Math.PI * 2 * i) / 9 + Math.random() * 0.5;
        const r = 34 + Math.random() * 26;
        s.style.setProperty('--dx', Math.cos(a) * r + 'px');
        s.style.setProperty('--dy', Math.sin(a) * r - 12 + 'px');
        s.style.setProperty('--sp', ['#8ab8ff', '#ffe28a', '#ff9ec4', '#8ff0c4'][i % 4]);
        s.style.left = cx - 3 + 'px';
        s.style.top = y + 'px';
        s.addEventListener('animationend', () => s.remove());
        setTimeout(() => s.remove(), 1400);
        this.layer.append(s);
      }
    }

    puff(M) {
      if (this.motion !== 'full') return;
      for (let i = 0; i < 2; i++) {
        const p = document.createElement('i');
        p.className = 'pet-puff';
        p.style.setProperty('--dx', (-this.dir * (10 + i * 12)) + 'px');
        p.style.left = this.cx - 7 - this.dir * (this.baseW * 0.35 + i * 8) + 'px';
        p.style.top = this.ground - 14 + 'px';
        p.addEventListener('animationend', () => p.remove());
        setTimeout(() => p.remove(), 1000);
        this.layer.append(p);
      }
    }

    destroy() { clearTimeout(this.timer); cancelAnimationFrame(this.raf); this.root.remove(); this.bubble.remove(); }
  }

  const api = { COLORS, ACCESSORIES, SIZES, DEFAULTS, PHRASES, sanitize, smooth, hopPose, hopTravel, createSlime, Companion };
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  else window.SuiPet = api;
})();
