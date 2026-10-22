'use strict';

// The October 2026 Node security release fixes Brotli, TLS and CA trust issues.
// Reject prereleases and unsupported release lines as well as older patches.
const floors = new Map([[22, [23, 4]], [24, [22, 1]], [26, [12, 1]]]);
function assertPatchedNodeVersion(version) {
  const match = /^(\d+)\.(\d+)\.(\d+)$/.exec(version);
  const [major, minor, patch] = match ? match.slice(1).map(Number) : [];
  const floor = floors.get(major);
  if (!floor || minor < floor[0] || (minor === floor[0] && patch < floor[1])) {
    throw new Error(`Defuddle requires patched Node.js 22.23.4+, 24.22.1+ or 26.12.1+ on those release lines; found ${version}. Update Shift and its web-extraction pack, or update your system Node.js.`);
  }
}
module.exports = { assertPatchedNodeVersion };
if (require.main === module) {
  try { assertPatchedNodeVersion(process.versions.node); }
  catch (error) { console.error(error.message); process.exitCode = 1; }
}
