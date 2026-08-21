// Single source of truth for the app version.
//
// The canonical version lives in the repo-root `VERSION` file. This script
// propagates it into the three places the toolchain reads (STANDARDS sec 5.2) and
// verifies they agree, so a human bumps the version in exactly one place:
//
//   - package.json               "version"
//   - src-tauri/tauri.conf.json  "version"        (Tauri / future updater)
//   - Cargo.toml                 [workspace.package] version  (-> CARGO_PKG_VERSION)
//
// Usage:
//   node scripts/version.js current                 # print the canonical version
//   node scripts/version.js check                   # fail if anything is out of sync (CI)
//   node scripts/version.js sync                     # stamp VERSION into the three files
//   node scripts/version.js set <x.y.z>             # set VERSION then sync
//   node scripts/version.js bump <patch|minor|major> # increment VERSION then sync

import fs from 'node:fs';

const VERSION_FILE = 'VERSION';
const SEMVER = /^\d+\.\d+\.\d+$/;

// Each target's version is matched as (prefix)(x.y.z)(suffix) so we can rewrite
// just the number and leave the rest of the file untouched. The Cargo.toml regex
// is anchored to the start of a line, so it matches `[workspace.package]`'s
// `version = "..."` and never a dependency's inline `{ version = "2" }`.
const TARGETS = [
  { path: 'package.json', re: /("version"\s*:\s*")(\d+\.\d+\.\d+)(")/ },
  { path: 'src-tauri/tauri.conf.json', re: /("version"\s*:\s*")(\d+\.\d+\.\d+)(")/ },
  { path: 'Cargo.toml', re: /(^version\s*=\s*")(\d+\.\d+\.\d+)(")/m },
];

// package-lock.json carries the root version too (top-level + packages[""]); npm
// ci fails if it drifts from package.json. It has 100+ dependency `version`
// fields, so it's edited JSON-aware (just the two root ones), not by regex.
const LOCK_FILE = 'package-lock.json';

function die(msg) {
  console.error(`version: ${msg}`);
  process.exit(1);
}

function canonical() {
  if (!fs.existsSync(VERSION_FILE)) die(`${VERSION_FILE} not found`);
  const v = fs.readFileSync(VERSION_FILE, 'utf8').trim();
  if (!SEMVER.test(v)) die(`${VERSION_FILE} contains "${v}", which is not an x.y.z version`);
  return v;
}

function extract(target) {
  const text = fs.readFileSync(target.path, 'utf8');
  const m = text.match(target.re);
  if (!m) die(`could not find a version in ${target.path}`);
  return m[2];
}

function stamp(target, version) {
  const text = fs.readFileSync(target.path, 'utf8');
  if (!target.re.test(text)) die(`could not find a version to replace in ${target.path}`);
  fs.writeFileSync(target.path, text.replace(target.re, `$1${version}$3`));
}

/** Update package-lock.json's root version fields (JSON-aware). No-op if absent. */
function stampLock(version) {
  if (!fs.existsSync(LOCK_FILE)) return;
  const lock = JSON.parse(fs.readFileSync(LOCK_FILE, 'utf8'));
  lock.version = version;
  if (lock.packages && lock.packages['']) lock.packages[''].version = version;
  // Match npm's on-disk format: 2-space indent + trailing newline.
  fs.writeFileSync(LOCK_FILE, `${JSON.stringify(lock, null, 2)}\n`);
}

function syncedPaths(version) {
  const paths = TARGETS.map((t) => t.path);
  stampLock(version);
  if (fs.existsSync(LOCK_FILE)) paths.push(LOCK_FILE);
  return paths;
}

function sync(version) {
  for (const t of TARGETS) stamp(t, version);
  const paths = syncedPaths(version);
  console.log(`version: stamped ${version} into ${paths.join(', ')}`);
}

function setVersion(v) {
  if (!SEMVER.test(v)) die(`"${v}" is not an x.y.z version`);
  fs.writeFileSync(VERSION_FILE, `${v}\n`);
  sync(v);
}

function bump(level) {
  const [maj, min, pat] = canonical().split('.').map(Number);
  let next;
  if (level === 'major') next = `${maj + 1}.0.0`;
  else if (level === 'minor') next = `${maj}.${min + 1}.0`;
  else if (level === 'patch') next = `${maj}.${min}.${pat + 1}`;
  else die(`unknown bump level "${level}" (use patch | minor | major)`);
  setVersion(next);
  return next;
}

function check() {
  const v = canonical();
  const wrong = TARGETS.filter((t) => extract(t) !== v);
  for (const t of wrong) console.error(`version: ${t.path} has ${extract(t)}, expected ${v}`);
  if (fs.existsSync(LOCK_FILE)) {
    const lockV = JSON.parse(fs.readFileSync(LOCK_FILE, 'utf8')).version;
    if (lockV !== v) {
      console.error(`version: ${LOCK_FILE} has ${lockV}, expected ${v}`);
      wrong.push(LOCK_FILE);
    }
  }
  if (wrong.length) die('version mismatch - run `node scripts/version.js sync` and commit');
  console.log(`version: all in sync at ${v}`);
}

/** Expose a value to later GitHub Actions steps when running in CI. */
function emitOutput(key, value) {
  if (process.env.GITHUB_OUTPUT) fs.appendFileSync(process.env.GITHUB_OUTPUT, `${key}=${value}\n`);
}

const [cmd, arg] = process.argv.slice(2);
switch (cmd) {
  case 'current':
    console.log(canonical());
    break;
  case 'check':
    check();
    break;
  case 'sync':
    sync(canonical());
    break;
  case 'set':
    setVersion(arg);
    emitOutput('version', canonical());
    break;
  case 'bump':
    bump(arg);
    emitOutput('version', canonical());
    break;
  default:
    die('usage: version.js <current|check|sync|set x.y.z|bump patch|minor|major>');
}
