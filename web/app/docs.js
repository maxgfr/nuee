// @ts-check
// Documentation pages: a Copy button after each command block.
// The pages read fine without it; it only adds the buttons.
import { copyButton } from './lib/dom.js';

for (const pre of document.querySelectorAll('pre.command')) {
  const block = document.createElement('div');
  block.className = 'command-block';
  pre.replaceWith(block);
  block.append(pre, copyButton(() => (pre.textContent ?? '').trim()));
}

// The contents fold away on narrow screens and stay open in the margin column on wide ones.
const fold = document.querySelector('details.toc-fold');
const summary = fold?.querySelector('summary');
if (fold instanceof HTMLDetailsElement && summary) {
  const wide = window.matchMedia('(min-width: 64rem)');
  const apply = () => {
    if (wide.matches) fold.open = true;
    summary.tabIndex = wide.matches ? -1 : 0;
  };
  summary.addEventListener('click', (event) => {
    if (wide.matches) event.preventDefault();
  });
  wide.addEventListener('change', apply);
  apply();
}
