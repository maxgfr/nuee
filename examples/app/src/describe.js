// One sentence about a build, from the note build.mjs leaves in the bundle.

/** @param {{ commit: string, builtAt: string, node: string, platform: string }} info */
export function describe(info) {
  const when = new Date(info.builtAt).toUTCString();
  return `Built from commit ${info.commit.slice(0, 12)} on ${info.platform} with Node ${info.node}, ${when}.`;
}
