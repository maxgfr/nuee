// The app: a counter, and where this copy of the app was built.
import { describe } from './describe.js';

/* global BUILD_INFO */
const info = /** @type {{ commit: string, builtAt: string, node: string, platform: string }} */ (BUILD_INFO);

const where = document.getElementById('built');
if (where) where.textContent = describe(info);

const button = document.getElementById('count');
let clicks = 0;
button?.addEventListener('click', () => {
  clicks += 1;
  button.textContent = `Clicked ${clicks} time${clicks === 1 ? '' : 's'}`;
});
