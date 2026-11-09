// The sample site's own script: it only runs where the site is served with its scripts, as
// through the loader, never in Nuée's inert preview.
const where = document.getElementById('where');
if (where) where.textContent = `This script runs on ${location.origin}, served from the group's machines.`;
const button = document.getElementById('count');
let clicks = 0;
button?.addEventListener('click', () => {
  clicks += 1;
  button.textContent = `Clicked ${clicks} time${clicks === 1 ? '' : 's'}`;
});
