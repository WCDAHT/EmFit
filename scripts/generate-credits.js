import { execSync } from 'child_process';
import fs from 'fs';
import path from 'path';

// Best-effort third-party license aggregation for the About panel.
//
// This MUST NOT fail the build. The output file is imported by About.svelte and
// is gitignored (so it only exists if we write it), so each source is gathered
// independently and the file is always written. A missing tool — most commonly
// `cargo-about` not being installed (CI, a fresh clone) — degrades to a note
// rather than aborting `npm run build`.

const OUT_PATH = './frontend/src/assets/generated-licenses.txt';

/** Scan node_modules for each runtime dependency's license file. */
function gatherJsLicenses() {
  try {
    const pkg = JSON.parse(fs.readFileSync('./package.json', 'utf8'));
    const deps = Object.keys(pkg.dependencies || {});
    let out = '';
    for (const dep of deps) {
      try {
        const depPath = path.join('./node_modules', dep);
        if (!fs.existsSync(depPath)) continue;
        const file = fs.readdirSync(depPath).find((f) => {
          const l = f.toLowerCase();
          return l.includes('license') || l.includes('licence') || l.includes('copying');
        });
        if (!file) continue;
        const content = fs.readFileSync(path.join(depPath, file), 'utf8');
        out += `PACKAGE: ${dep}\n${content.trim()}\n${'='.repeat(60)}\n\n`;
      } catch (e) {
        console.warn(`  (skipped ${dep}: ${e.message})`);
      }
    }
    return out || '(no bundled JavaScript dependency licenses found)\n';
  } catch (e) {
    console.warn(`  (JavaScript license scan failed: ${e.message})`);
    return '(JavaScript dependency licenses unavailable)\n';
  }
}

/** Run cargo-about for the Rust dependency licenses; note its absence if missing. */
function gatherRustLicenses() {
  const tmp = path.join('.', 'temp-rust-licenses.txt');
  try {
    execSync('cargo about generate about.txt.hbs -o temp-rust-licenses.txt', { stdio: 'pipe' });
    const text = fs.readFileSync(tmp, 'utf8');
    return text;
  } catch (e) {
    console.warn('  (cargo-about unavailable; Rust dependency licenses not generated.');
    console.warn('   Install it with `cargo install cargo-about` to include them.)');
    console.warn(`   ${(e.message || '').split('\n')[0]}`);
    return '(Rust dependency licenses were not generated — cargo-about is not installed.)\n';
  } finally {
    try {
      if (fs.existsSync(tmp)) fs.unlinkSync(tmp);
    } catch {
      /* ignore */
    }
  }
}

console.log('Gathering JS licenses (manual scan)...');
const jsLicenses = gatherJsLicenses();
console.log('Generating Rust licenses (cargo-about)...');
const rustLicenses = gatherRustLicenses();

const combined =
  `
THIRD-PARTY SOFTWARE NOTICES
================================================================================
This application bundles various open-source components. Their licenses are
listed below.
================================================================================

--- JAVASCRIPT DEPENDENCIES ---

${jsLicenses}

--- RUST DEPENDENCIES ---

${rustLicenses}
`.trim() + '\n';

try {
  fs.mkdirSync(path.dirname(OUT_PATH), { recursive: true });
  fs.writeFileSync(OUT_PATH, combined);
  console.log(`✅ Licenses written to ${OUT_PATH}`);
} catch (e) {
  // Even this is non-fatal: if a stale file already exists the build can still
  // proceed; only warn so the build isn't blocked by a credits hiccup.
  console.warn(`⚠️  Could not write ${OUT_PATH}: ${e.message}`);
}
