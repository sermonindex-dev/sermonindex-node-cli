// Kiosk render check. Run before every CLI release (CI does it on each tag):
//   cd tests/kiosk && npm ci && npm test
// It exists because 0.3.1 shipped a ReferenceError in render() that blanked every
// kiosk screen, and a syntax check cannot see one. Only executing the code can.
// Executes the kiosk dashboard's render() on every page against sample /stats
// payloads. A syntax check cannot see a ReferenceError; only running it can.
const fs = require('fs');
const { JSDOM, VirtualConsole } = require('jsdom');
const file = process.argv[2];
const html = fs.readFileSync(file, 'utf8');

const now = Math.floor(Date.now() / 1000);
const FULL = {
  node: { name: 'test', version: '0.3.2', port: 8137, peers: 4, uptime_s: 3600,
    coverage_pct: 99.9, held: 40042, catalog: 40042, storage_bytes: 9e10,
    uploaded_bytes: 1e9, reachable: 'open', quiet: false, disk_full: false,
    seed_granted: true, category: 'seed', source_mode: 'hybrid',
    swarm_files: 12, swarm_bytes: 1e8, http_files: 3, http_bytes: 2e7,
    peers_in: 2, peers_out: 2, peers_in_peak: 5, v6_inbound_seen: false, available: true, update_available: '0.3.3' },
  system: { cpu_pct: 12, mem_used: 1e9, mem_total: 4e9, disk_used: 9e10, disk_total: 2e11,
    temp_c: 51, load: [0.5, 0.4, 0.3], os: 'Linux', arch: 'aarch64', hostname: 'pi' },
  trends: { traffic: [{ ts: now - 86400, inb: 10, out: 20 }, { ts: now, inb: 5, out: 8 }] },
  network: { online_count: 3, total_count: 5, total_sermons: 40042, countries: 2,
    nodes: [{ self: true, city: 'Boca Raton', country: 'US', sermons: 40042, lat: 26.3, lon: -80.1, category: 'seed' },
            { city: 'Miami', country: 'US', sermons: 40042, lat: 25.7, lon: -80.2, category: 'node' }] },
};
const MINIMAL = { node: {} };                          // first seconds after start
const NOSOURCE = JSON.parse(JSON.stringify(FULL)); delete NOSOURCE.node.source_mode;

async function runCase(label, payload) {
  const errors = [];
  const vc = new VirtualConsole();
  vc.on('jsdomError', e => errors.push(String(e.message || e)));
  vc.on('error', e => errors.push(String(e)));
  const dom = new JSDOM(html, {
    runScripts: 'dangerously', pretendToBeVisual: true, virtualConsole: vc,
    url: 'http://127.0.0.1:8137/',
    beforeParse(w) {
      w.fetch = async () => ({ ok: true, status: 200, json: async () => payload, text: async () => '' });
      w.matchMedia = w.matchMedia || (() => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {} }));
      w.addEventListener('error', ev => errors.push('window.onerror: ' + (ev.error && ev.error.message || ev.message)));
    },
  });
  const w = dom.window;
  await new Promise(r => setTimeout(r, 300));
  const pages = [...w.document.querySelectorAll('.nav button[data-p]')].map(b => b.dataset.p);
  const out = [];
  for (const p of pages) {
    w.document.querySelectorAll('.nav button').forEach(b => b.classList.toggle('on', b.dataset.p === p));
    try {
      w.eval('S=' + JSON.stringify(payload) + ';render();');
      const pane = w.document.getElementById('p_' + p);
      const len = pane ? pane.innerHTML.length : -1;
      out.push(p + ':' + (len > 50 ? 'drawn(' + len + ')' : 'EMPTY(' + len + ')'));
      if (len <= 50) errors.push('page ' + p + ' drew nothing');
    } catch (e) {
      out.push(p + ':THREW');
      errors.push('page ' + p + ': ' + e.message);
    }
  }
  const src = w.document.getElementById('srcmode');
  out.push('srcmode="' + (src ? src.textContent : 'n/a') + '"');
  // Update notice: shown exactly when /stats says a newer release exists.
  const up = w.document.getElementById('updpill');
  const wantUp = !!(payload.node && payload.node.update_available);
  const shown = !!up && up.style.display !== 'none';
  out.push('update=' + (shown ? '"' + up.textContent + '"' : 'hidden'));
  if (shown !== wantUp) errors.push('update pill ' + (shown ? 'shown with no update' : 'missing for an available update'));
  w.close();
  return { label, out, errors };
}

(async () => {
  let failed = 0;
  for (const [label, payload] of [['full', FULL], ['minimal', MINIMAL], ['no-source-mode', NOSOURCE]]) {
    const r = await runCase(label, payload);
    console.log((r.errors.length ? 'FAIL ' : 'ok   ') + label.padEnd(15) + r.out.join('  '));
    r.errors.forEach(e => console.log('       x ' + e));
    if (r.errors.length) failed++;
  }
  process.exit(failed ? 1 : 0);
})();
