// @ts-check
// A small flock on a canvas: one bird per peer (or a demo flock). It flies for a few seconds,
// then settles back on the still murmuration silhouette and stays there (nothing moves for
// longer than five seconds). It stops drawing while the page is hidden or the canvas is out of
// view, and under reduced motion it is only ever drawn as the silhouette.

/** Visible flight time before the flock is still, in ms. */
export const FLIGHT_MS = 5000;
/** The last part of the flight, when every bird glides back to its place in the silhouette. */
export const SETTLE_MS = 1500;

/**
 * Where the flight is after `elapsed` ms of visible animation: `pull` (0..1) is how far each
 * bird is drawn towards its silhouette place on this frame, `done` ends the animation.
 * @param {number} elapsed
 * @returns {{ pull: number, done: boolean }}
 */
export function flightPhase(elapsed) {
  if (elapsed >= FLIGHT_MS) return { pull: 1, done: true };
  const into = elapsed - (FLIGHT_MS - SETTLE_MS);
  if (into <= 0) return { pull: 0, done: false };
  // Eases in: a light pull at first, all the way home at the end.
  const p = into / SETTLE_MS;
  return { pull: p * p * 0.35, done: false };
}

/**
 * `ink` (0..7) draws the bird in that device ink of the page (--ink-0 … --ink-7).
 * @typedef {{ id: string, hue: number, ink?: number }} BirdSpec
 * @typedef {{ id: string, hue: number, ink?: number, x: number, y: number, vx: number, vy: number }} Bird
 */

/** Deterministic pseudo-random numbers, so the still frame is always the same. @param {number} seed */
function rng(seed) {
  let s = seed >>> 0 || 1;
  return () => {
    s ^= s << 13;
    s ^= s >>> 17;
    s ^= s << 5;
    return (s >>> 0) / 4294967296;
  };
}

/**
 * @param {HTMLCanvasElement} canvas
 * @param {{ birds?: BirdSpec[] }} [opts]
 */
export function murmuration(canvas, opts = {}) {
  const ctx = canvas.getContext('2d');
  const reduce = window.matchMedia('(prefers-reduced-motion: reduce)');
  let rand = rng(7);
  /** @type {BirdSpec[]} */
  let specs = [];
  /** @type {Bird[]} */
  let birds = [];
  let frame = 0;
  let running = false;
  let width = 0;
  let height = 0;
  let inView = true;
  /** Visible flight so far; once it reaches FLIGHT_MS the flock stays still. */
  let flown = 0;
  let last = 0;
  /** Each bird's place in the still silhouette, by id. @type {Map<string, { x: number, y: number, vx: number, vy: number }>} */
  let home = new Map();
  /** Device inks, read from the page once and again when the color scheme changes. @type {string[]} */
  let palette = [];
  const scheme = window.matchMedia('(prefers-color-scheme: dark)');
  const readInks = () => {
    const style = getComputedStyle(canvas);
    palette = Array.from({ length: 8 }, (_, k) => style.getPropertyValue(`--ink-${k}`).trim());
  };

  function resize() {
    const ratio = Math.min(window.devicePixelRatio || 1, 2);
    width = canvas.clientWidth || 300;
    height = canvas.clientHeight || 150;
    canvas.width = Math.round(width * ratio);
    canvas.height = Math.round(height * ratio);
    if (ctx) ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
  }

  /** @param {BirdSpec[]} next */
  function setBirds(next) {
    specs = next;
    layoutHome();
    const old = new Map(birds.map((b) => [b.id, b]));
    birds = next.map((s) => {
      const b = old.get(s.id);
      if (b) return { ...b, hue: s.hue, ink: s.ink };
      const p = silhouette(rand, width, height);
      return { id: s.id, hue: s.hue, ink: s.ink, ...p };
    });
    if (!running) draw();
  }

  function step() {
    const n = birds.length;
    for (const b of birds) {
      let cx = 0;
      let cy = 0;
      let ax = 0;
      let ay = 0;
      let sx = 0;
      let sy = 0;
      for (const o of birds) {
        cx += o.x;
        cy += o.y;
        ax += o.vx;
        ay += o.vy;
        const dx = b.x - o.x;
        const dy = b.y - o.y;
        const d2 = dx * dx + dy * dy;
        if (o !== b && d2 < 400) {
          sx += dx / (d2 + 1);
          sy += dy / (d2 + 1);
        }
      }
      b.vx += (cx / n - b.x) * 0.0008 + (ax / n - b.vx) * 0.05 + sx * 0.6 + (rand() - 0.5) * 0.08;
      b.vy += (cy / n - b.y) * 0.0008 + (ay / n - b.vy) * 0.05 + sy * 0.6 + (rand() - 0.5) * 0.08;
      // Stay inside the canvas with a soft push from the edges.
      const m = 24;
      if (b.x < m) b.vx += 0.1;
      if (b.x > width - m) b.vx -= 0.1;
      if (b.y < m) b.vy += 0.1;
      if (b.y > height - m) b.vy -= 0.1;
      const speed = Math.hypot(b.vx, b.vy);
      const max = 1.6;
      if (speed > max) {
        b.vx = (b.vx / speed) * max;
        b.vy = (b.vy / speed) * max;
      }
      b.x += b.vx;
      b.y += b.vy;
    }
  }

  function draw() {
    if (!ctx) return;
    ctx.clearRect(0, 0, width, height);
    ctx.lineWidth = 1.5;
    ctx.lineCap = 'round';
    ctx.lineJoin = 'round';
    for (const b of birds) {
      const color = (b.ink !== undefined && palette[b.ink % 8]) || `hsl(${b.hue} 55% 40%)`;
      // A gull seen from below: two wing strokes, each bowed up, meeting at the body. The
      // wingspan (about 12px) lies across the direction of flight, tilted with it.
      const tilt = Math.max(-0.5, Math.min(0.5, Math.atan2(b.vy, Math.abs(b.vx)))) * 0.6;
      const cos = Math.cos(tilt);
      const sin = Math.sin(tilt);
      const pt = (/** @type {number} */ dx, /** @type {number} */ dy) => /** @type {[number, number]} */ ([b.x + dx * cos - dy * sin, b.y + dx * sin + dy * cos]);
      ctx.strokeStyle = color;
      ctx.beginPath();
      ctx.moveTo(...pt(-6, 0.5));
      ctx.quadraticCurveTo(...pt(-3, -3), ...pt(0, 1));
      ctx.quadraticCurveTo(...pt(3, -3), ...pt(6, 0.5));
      ctx.stroke();
    }
  }

  /** The silhouette for the current size, from the same seed as the still frame. */
  function layoutHome() {
    const r = rng(7);
    home = new Map(specs.map((s) => [s.id, silhouette(r, width, height)]));
  }

  /** @param {number} now */
  function loop(now) {
    // Frames apart by more than a second (a background tab) count as one frame.
    flown += last ? Math.min(now - last, 50) : 16;
    last = now;
    const { pull, done } = flightPhase(flown);
    step();
    if (pull > 0) {
      for (const b of birds) {
        const p = home.get(b.id);
        if (!p) continue;
        b.x += (p.x - b.x) * pull;
        b.y += (p.y - b.y) * pull;
        b.vx += (p.vx - b.vx) * pull;
        b.vy += (p.vy - b.vy) * pull;
      }
    }
    if (done) {
      // Settled: every bird on its silhouette place, drawn once, still.
      running = false;
      birds = birds.map((b) => ({ ...b, ...(home.get(b.id) ?? {}) }));
      draw();
      return;
    }
    draw();
    frame = requestAnimationFrame(loop);
  }

  function update() {
    const shouldRun = !reduce.matches && !document.hidden && canvas.isConnected && inView && flown < FLIGHT_MS;
    if (shouldRun && !running) {
      running = true;
      last = 0;
      frame = requestAnimationFrame(loop);
    } else if (!shouldRun && running) {
      running = false;
      cancelAnimationFrame(frame);
      draw();
    }
  }

  resize();
  readInks();
  // The flock starts as the silhouette, which is also the still frame under reduced motion.
  setBirds(opts.birds ?? []);
  draw();
  const onScheme = () => {
    readInks();
    draw();
  };
  scheme.addEventListener('change', onScheme);
  const io = new IntersectionObserver((entries) => {
    for (const e of entries) inView = e.isIntersecting;
    update();
  });
  io.observe(canvas);
  const ro = new ResizeObserver(() => {
    resize();
    layoutHome();
    // A still flock is laid out again for the new size, from the same seed.
    if (!running) {
      rand = rng(7);
      birds = [];
      setBirds(specs);
    }
    draw();
  });
  ro.observe(canvas);
  document.addEventListener('visibilitychange', update);
  reduce.addEventListener('change', update);
  update();

  return {
    setBirds,
    stop() {
      running = false;
      cancelAnimationFrame(frame);
      ro.disconnect();
      io.disconnect();
      scheme.removeEventListener('change', onScheme);
      document.removeEventListener('visibilitychange', update);
      reduce.removeEventListener('change', update);
    },
  };
}

/**
 * A point of a murmuration silhouette: a long band flowing across the whole width, thick and
 * dense in the middle, thinning to stragglers at both ends. Deterministic for a seeded `rand`.
 * @param {() => number} rand
 * @param {number} width
 * @param {number} height
 * @returns {{ x: number, y: number, vx: number, vy: number }}
 */
export function silhouette(rand, width, height) {
  // Positions along the band lean towards the middle (two uniforms averaged), with a few
  // stragglers spread evenly.
  const u = rand() < 0.8 ? (rand() + rand()) / 2 : rand();
  const t = 0.03 + u * 0.94;
  const wave = (/** @type {number} */ s) => 0.5 + 0.2 * Math.sin(s * Math.PI * 2.1 + 0.7) + 0.06 * Math.sin(s * Math.PI * 5.3);
  const centre = height * wave(t);
  // Thickness swells in two places and shrinks to a thread at the ends.
  const swell = Math.sin(Math.PI * t) ** 1.5 * (0.55 + 0.45 * Math.sin(t * Math.PI * 3 + 0.4) ** 2);
  const spread = height * 0.13 * swell + 3;
  const off = (rand() + rand() + rand() - 1.5) / 1.5;
  const slope = (wave(t + 0.01) - wave(t - 0.01)) / 0.02;
  return { x: width * t, y: Math.max(8, Math.min(height - 8, centre + off * spread)), vx: 1, vy: (slope * height) / width };
}

/** A demo flock of `n` birds in the device inks. @param {number} n @returns {BirdSpec[]} */
export function demoBirds(n) {
  return Array.from({ length: n }, (_, i) => ({ id: `demo-${i}`, hue: 200 + ((i * 37) % 60), ink: (i * 3) % 8 }));
}
