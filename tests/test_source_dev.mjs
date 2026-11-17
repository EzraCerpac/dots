import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { syncSource, withRepositoryLock } from '../lib/source-repo.mjs';
import { miseBinary, requires } from './lib/prereq.mjs';

// Every sync runs the encrypted-history guard, which reads mise's inventory with jq.
const available = requires('git', 'jq');
function run(bin, args, cwd, options = {}) {
  const r = spawnSync(bin, args, { cwd, encoding: 'utf8', ...options });
  assert.equal(r.status, 0, `${bin} ${args.join(' ')}: ${r.stderr}`);
  return r.stdout.trim();
}
const git = (cwd, ...args) => run('git', args, cwd);
const read = (root, file) => fs.readFileSync(path.join(root, file), 'utf8');
function failIndexInstall(root, sync, afterCheckout = () => {}) {
  const index = path.join(git(root, 'rev-parse', '--absolute-git-dir'), 'index');
  const rename = fs.renameSync;
  let injected = false, result;
  fs.renameSync = (from, to) => {
    if (from === `${index}.lock` && to === index) {
      afterCheckout();
      injected = true;
      throw new Error('injected index installation failure');
    }
    return rename(from, to);
  };
  try { result = sync(); } finally { fs.renameSync = rename; }
  assert(injected);
  assert.equal(result.code, 0);
  assert.match(result.text, /NOT APPLIED: injected index installation failure; checkout pending/);
  return result;
}

// Flags any added line containing FAKE_SECRET, reported like gitleaks' JSON.
const FAKE_GITLEAKS = `#!/usr/bin/env node
const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const args = process.argv.slice(2);
const report = args[args.indexOf('--report-path') + 1];
const opts = args.find(a => a.startsWith('--log-opts=')).slice(11).split(' ');
const root = args[args.length - 1];
const log = spawnSync('git', ['log', '-p', '--format=commit %H', ...opts], { cwd: root, encoding: 'utf8' }).stdout;
const findings = [];
let commit, file, line;
for (const text of log.split('\\n')) {
  if (text.startsWith('commit ')) commit = text.slice(7);
  else if (text.startsWith('+++ ')) file = text.slice(6);
  else if (text.startsWith('@@')) line = Number(text.match(/\\+(\\d+)/)[1]);
  else if (text.startsWith('+')) { if (text.includes('FAKE_SECRET')) findings.push({ File: file, StartLine: line, RuleID: 'fake-rule', Commit: commit }); line++; }
  else if (text.startsWith(' ')) line++;
}
fs.writeFileSync(report, JSON.stringify(findings));
process.exit(findings.length ? 1 : 0);
`;

function fixture(t) {
  const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'dots-sync-test-')));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const oldEnv = { ...process.env };
  t.after(() => { for (const k of Object.keys(process.env)) if (!(k in oldEnv)) delete process.env[k]; Object.assign(process.env, oldEnv); });
  fs.writeFileSync(path.join(dir, 'gitconfig'), '[user]\nname = Fixture\nemail = fixture@example.invalid\n[init]\ndefaultBranch = main\n');
  process.env.GIT_CONFIG_GLOBAL = path.join(dir, 'gitconfig');
  process.env.GIT_CONFIG_NOSYSTEM = '1';
  const origin = path.join(dir, 'origin.git'), a = path.join(dir, 'a'), b = path.join(dir, 'b');
  git(dir, 'init', '--bare', '--initial-branch=main', origin);
  git(dir, 'clone', '-q', origin, a);
  fs.writeFileSync(path.join(a, 'config.toml'), '# initial\n');
  fs.writeFileSync(path.join(a, 'text'), 'one\ntwo\nthree\nfour\nfive\nsix\nseven\n');
  fs.writeFileSync(path.join(a, '.gitignore'), '/config.local.toml\n/miserc.toml\n/config.host-*.toml\n/mise.lock\n/.setup-state/\n/locks/\n');
  git(a, 'add', '.'); git(a, 'commit', '-qm', 'Initial'); git(a, 'push', '-q', 'origin', 'main');
  git(a, 'switch', '-qc', 'dev'); git(a, 'push', '-q', 'origin', 'dev');
  git(dir, 'clone', '-q', '--branch', 'dev', origin, b);
  const miseBin = path.join(dir, 'mise');
  fs.writeFileSync(miseBin, `#!/bin/sh
set -eu
case "$*" in
  *'dotfiles paths --json'*) if [ -n "\${DEV_TEST_PATHS:-}" ]; then echo "$DEV_TEST_PATHS"; else echo '{"entries": []}'; fi ;;
  *'dotfiles status --json'*) echo '{"files": [], "edits": []}' ;;
  *'.encrypt'*) exit 1 ;;
  *'dotfiles save'*)
    if [ -n "\${DEV_TEST_SAVE_FAIL:-}" ]; then echo "$DEV_TEST_SAVE_FAIL" >&2; exit 1; fi
    if [ -n "\${DEV_TEST_SAVED:-}" ]; then touch "$DEV_TEST_SAVED"; fi ;;
  *'config ls'*)
    if [ -n "\${DEV_TEST_MUTATE:-}" ]; then echo late > "$DEV_TEST_MUTATE"; fi
    if grep -q INVALID "$2/config.toml"; then echo 'invalid config' >&2; exit 1; fi ;;
  *'dotfiles apply'*)
    case "$*" in *--dry-run*) ;; *)
      if [ -n "\${DEV_TEST_APPLY_FAIL:-}" ]; then echo 'applying 3 dotfiles'; echo "$DEV_TEST_APPLY_FAIL" >&2; exit 1; fi ;;
    esac
    # Like mise: every source a local declaration names must exist.
    for file in "$2"/config.local.toml "$2"/config.host-*.toml; do
      [ -f "$file" ] || continue
      for source in $(sed -n 's/^source = "\\(.*\\)"$/\\1/p' "$file"); do
        [ -e "$2/$source" ] || { echo "sources do not exist: $source" >&2; exit 1; }
      done
    done ;;
esac
`);
  fs.chmodSync(miseBin, 0o755);
  const gitleaks = path.join(dir, 'gitleaks');
  fs.writeFileSync(gitleaks, FAKE_GITLEAKS);
  fs.chmodSync(gitleaks, 0o755);
  const sync = (root, options = {}) => {
    const lines = [];
    const code = syncSource(root, { expectedRemote: origin, miseBin, gitleaks, print: text => lines.push(text), ...options });
    return { code, text: lines.join('\n') };
  };
  const write = (root, file, contents) => { fs.mkdirSync(path.dirname(path.join(root, file)), { recursive: true }); fs.writeFileSync(path.join(root, file), contents); };
  const dev = root => git(root, 'rev-parse', 'refs/heads/dev');
  const remote = () => git(origin, 'rev-parse', 'dev');
  const clean = root => assert.equal(git(root, 'status', '--porcelain'), '', `${root} has uncommitted changes`);
  return { dir, a, b, origin, sync, write, dev, remote, clean, gitleaks };
}

// An older host: its tree still has the Herdr template and its local config the
// declaration an old setup script appended. The incoming commit archives the
// template and, when LISTED, retires the declaration.
const HERDR = '\n[dotfiles.herdr]\nsource = "templates/herdr.tera"\nmode = "template"\nvariants = [{ os = "macos", target = "~/.config/herdr/config.toml" }, { os = "linux", target = "~/.config/herdr/config.toml" }]\n';
const KEPT = '# this machine\n[env]\nSETUP_PROFILE = "laptop"\n';
function olderHost(f, { listed = true, file = 'config.local.toml', local = KEPT + HERDR } = {}) {
  f.write(f.b, 'templates/herdr.tera', 'theme = "{{ \'dark\' }}"\n'); f.sync(f.b); f.sync(f.a);
  assert(fs.existsSync(path.join(f.a, 'templates/herdr.tera')));
  f.write(f.a, file, local); fs.chmodSync(path.join(f.a, file), 0o600);
  fs.rmSync(path.join(f.b, 'templates'), { recursive: true });
  if (listed) f.write(f.b, 'dots/retired-declarations', '# Retired local declarations.\ndotfiles.herdr\ttemplates/herdr.tera\n');
  assert.match(f.sync(f.b).text, /pushed/);
  return local;
}
const unchangedLocal = (f, file, local, stat) => {
  assert.equal(read(f.a, file), local);
  assert.equal(fs.statSync(path.join(f.a, file)).mtimeMs, stat.mtimeMs);
  assert(!fs.existsSync(path.join(f.a, '.git/dots-retired')), 'no backup without a removal');
};
const RETIRED = file => new RegExp(`\\n {2}RETIRED local declaration \\[dotfiles\\.herdr\\] from ${file.replace(/\./g, '\\.')} \\(source templates/herdr\\.tera no longer exists\\); deployed files left in place, backup (\\.git/dots-retired/${file.replace(/\./g, '\\.')}\\.\\d{8}T\\d{6}Z)$`);

test('a clean checkout syncs as a no-op', available, t => {
  const f = fixture(t); const before = f.dev(f.a);
  const { code, text } = f.sync(f.a);
  assert.equal(code, 0);
  assert.equal(text, 'dots sync: no local changes · pulled dev +0, main +0 · nothing to push');
  assert.equal(f.dev(f.a), before); assert.equal(f.remote(), before);
});

test('a local edit is shared and private files stay local', available, t => {
  const f = fixture(t);
  f.write(f.a, 'new', 'ordinary public edit\n');
  f.write(f.a, 'config.local.toml', '[env]\nPRIVATE="yes"\n');
  // Directory-only ignore rules (/.setup-state/, /locks/) must cover real directories.
  f.write(f.a, '.setup-state/state.json', '{}\n');
  f.write(f.a, 'locks/tool.lock', 'held\n');
  const { text } = f.sync(f.a);
  assert.match(text, /^dots sync: shared 1 file · pulled dev \+0, main \+0 · pushed [0-9a-f]{7}$/);
  assert.equal(f.remote(), f.dev(f.a));
  assert.equal(git(f.a, 'symbolic-ref', 'HEAD'), 'refs/heads/dev');
  assert.doesNotMatch(git(f.a, 'ls-tree', '-r', '--name-only', 'dev'), /config\.local\.toml|\.setup-state|locks/);
  assert.match(git(f.a, 'log', '-1', '--format=%s', 'dev'), /^Save local setup \(/);
  f.clean(f.a);
});

test('sync saves nothing while a tracked file is not declared encrypted', available, t => {
  const f = fixture(t);
  const saved = path.join(f.dir, 'saved');
  process.env.DEV_TEST_SAVED = saved;
  const { text } = f.sync(f.a);
  assert(fs.existsSync(saved), 'an encrypted (here: empty) inventory is saved');
  fs.rmSync(saved);
  process.env.DEV_TEST_PATHS = JSON.stringify({ entries: [{ path: '~/.app', mode: 'track', declared_in: path.join(f.a, 'config.toml') }] });
  const refused = f.sync(f.a);
  assert.equal(text, 'dots sync: no local changes · pulled dev +0, main +0 · nothing to push');
  assert.match(refused.text, /\n {2}SAVE REFUSED: history entry ~\/\.app in .*config\.toml is not encrypted; set encrypt = true there; nothing saved$/);
  assert(!fs.existsSync(saved), 'mise bootstrap dotfiles save ran');
});

test('a remote change arrives on disk', available, t => {
  const f = fixture(t);
  f.write(f.b, 'remote', 'from b\n'); f.sync(f.b);
  const { text } = f.sync(f.a);
  assert.match(text, /^dots sync: no local changes · pulled dev \+1, main \+0 · nothing to push\n {2}changed on disk: 1 file changed/);
  assert.equal(read(f.a, 'remote'), 'from b\n');
  assert.equal(f.dev(f.a), f.remote()); f.clean(f.a);
});

test('dots up sees the configuration source move as one change event', available, t => {
  const f = fixture(t); const before = f.dev(f.a);
  f.write(f.b, 'remote', 'from b\n'); f.sync(f.b);
  const events = path.join(f.dir, 'events');
  const fd = fs.openSync(events, 'w');
  process.env.DOTS_UP_EVENT_FD = String(fd);
  try { f.sync(f.a); f.sync(f.a); } finally { delete process.env.DOTS_UP_EVENT_FD; fs.closeSync(fd); }
  const changes = fs.readFileSync(events, 'utf8').trim().split('\n').map(line => JSON.parse(line));
  assert.deepEqual(changes, [{ v: 1, kind: 'change', stage: 'source-sync', item: 'Configuration source', action: 'updated', before, after: f.dev(f.a) }]);
});

test('non-conflicting edits on both sides merge', available, t => {
  const f = fixture(t);
  f.write(f.a, 'text', 'ONE\ntwo\nthree\nfour\nfive\nsix\nseven\n');
  f.write(f.b, 'text', 'one\ntwo\nthree\nfour\nfive\nsix\nSEVEN\n'); f.sync(f.b);
  const { text } = f.sync(f.a);
  assert.doesNotMatch(text, /CONFLICT/);
  assert.equal(read(f.a, 'text'), 'ONE\ntwo\nthree\nfour\nfive\nsix\nSEVEN\n');
  assert.equal(git(f.origin, 'show', 'dev:text'), 'ONE\ntwo\nthree\nfour\nfive\nsix\nSEVEN');
  f.sync(f.b); assert.equal(read(f.b, 'text'), read(f.a, 'text'));
});

test('a conflicting hunk keeps the published side and the loser stays recoverable', available, t => {
  const f = fixture(t);
  f.write(f.a, 'text', 'mine\ntwo\nthree\nfour\nfive\nsix\nmy tail\n');
  f.write(f.b, 'text', 'theirs\ntwo\nthree\nfour\nfive\nsix\nseven\n'); f.sync(f.b);
  const { code, text } = f.sync(f.a);
  assert.equal(code, 0);
  assert.equal(read(f.a, 'text'), 'theirs\ntwo\nthree\nfour\nfive\nsix\nmy tail\n');
  const match = text.match(/CONFLICT kept published: text {2}\(yours: git show ([0-9a-f]{12}):text\)/);
  assert(match, text);
  assert.equal(git(f.a, 'show', `${match[1]}:text`).split('\n')[0], 'mine');
  git(f.origin, 'merge-base', '--is-ancestor', match[1], 'dev');
  f.clean(f.a);
});

test('a binary conflict takes the published file whole', available, t => {
  const f = fixture(t);
  f.write(f.a, 'blob.bin', Buffer.from([0, 1, 2, 3])); f.sync(f.a); f.sync(f.b);
  f.write(f.a, 'blob.bin', Buffer.from([0, 9, 9, 9]));
  f.write(f.b, 'blob.bin', Buffer.from([0, 7, 7, 7])); f.sync(f.b);
  const { text } = f.sync(f.a);
  assert.deepEqual([...fs.readFileSync(path.join(f.a, 'blob.bin'))], [0, 7, 7, 7]);
  assert.match(text, /CONFLICT kept published: blob\.bin/);
});

test('a file/directory conflict keeps the published side at its own path', available, t => {
  for (const [published, local] of [['collision', 'collision/child'], ['collision/child', 'collision']]) {
    const f = fixture(t);
    f.write(f.b, published, 'published\n'); f.sync(f.b);
    f.write(f.a, local, 'local\n');
    const { text } = f.sync(f.a);
    assert.equal(git(f.origin, 'ls-tree', '-r', '--name-only', 'dev', '--', 'collision'), published);
    assert.equal(read(f.a, published), 'published\n');
    const match = text.match(/CONFLICT kept published: collision {2}\(yours: git show ([0-9a-f]{12}):collision\)/);
    assert(match, text);
    assert.equal(git(f.a, 'show', `${match[1]}:${local}`), 'local');
    f.clean(f.a);
  }
});

test('reviewed main merges into dev', available, t => {
  const f = fixture(t);
  const c = path.join(f.dir, 'c'); git(f.dir, 'clone', '-q', f.origin, c);
  f.write(c, 'reviewed', 'merged PR\n'); git(c, 'add', '.'); git(c, 'commit', '-qm', 'Reviewed'); git(c, 'push', '-q', 'origin', 'main');
  const { text } = f.sync(f.a);
  assert.match(text, /pulled dev \+0, main \+1 · pushed/);
  assert.equal(read(f.a, 'reviewed'), 'merged PR\n');
  git(f.origin, 'merge-base', '--is-ancestor', 'main', 'dev');
});

test('offline work is queued as origin/dev..dev and pushed next time', available, t => {
  const f = fixture(t);
  fs.renameSync(f.origin, `${f.origin}.offline`);
  f.write(f.a, 'text', 'saved offline\n');
  const offline = f.sync(f.a);
  assert.equal(offline.code, 0);
  assert.match(offline.text, /^dots sync: saved 1 file locally · offline · not pushed\n {2}OFFLINE: 1 commits unpushed, retried next run$/);
  assert.equal(git(f.a, 'rev-list', '--count', 'origin/dev..dev'), '1'); f.clean(f.a);
  fs.renameSync(`${f.origin}.offline`, f.origin);
  const online = f.sync(f.a);
  assert.match(online.text, /^dots sync: no local changes · pulled dev \+0, main \+0 · pushed/);
  assert.equal(f.remote(), f.dev(f.a));
});

test('a push race is merged and retried', available, t => {
  const f = fixture(t);
  const hook = path.join(f.a, '.git/hooks/pre-push');
  fs.writeFileSync(hook, `#!/bin/sh
set -eu
rm "$0"
printf 'raced\\n' > '${f.b}/raced'
git -C '${f.b}' add raced
git -C '${f.b}' commit -qm Race
git -C '${f.b}' push -q origin HEAD:dev
`);
  fs.chmodSync(hook, 0o755);
  f.write(f.a, 'local', 'local\n');
  const { text } = f.sync(f.a);
  assert.match(text, /· pushed/);
  assert.equal(f.remote(), f.dev(f.a));
  assert.equal(read(f.a, 'raced'), 'raced\n'); assert.equal(read(f.a, 'local'), 'local\n');
});

// A pre-push hook stands in for whatever makes a push fail or hang.
const prePush = (f, body) => {
  const hook = path.join(f.a, '.git/hooks/pre-push');
  fs.writeFileSync(hook, `#!/bin/sh\n${body}\n`);
  fs.chmodSync(hook, 0o755);
};

test('a push that fails for another reason says why, not that dev kept moving', available, t => {
  const f = fixture(t); const before = f.remote();
  prePush(f, "echo 'remote: Invalid username or password.' >&2\necho \"fatal: Authentication failed for 'https://example.invalid/dots.git/'\" >&2\nexit 1");
  f.write(f.a, 'local', 'local\n');
  const { text } = f.sync(f.a);
  assert.match(text, /· not pushed\n {2}NOT PUSHED: git push failed: fatal: Authentication failed for 'https:\/\/example\.invalid\/dots\.git\/'\n {4}remote: Invalid username or password\.(\n|$)/);
  assert.doesNotMatch(text, /kept moving|OFFLINE/);
  assert.equal(f.remote(), before);
});

test('a push that fails and leaves origin unreachable keeps its reason next to OFFLINE', available, t => {
  const f = fixture(t);
  prePush(f, `mv '${f.origin}' '${f.origin}.gone'\necho 'fatal: unable to access origin: Could not resolve host' >&2\nexit 1`);
  f.write(f.a, 'local', 'local\n');
  const { text } = f.sync(f.a);
  fs.renameSync(`${f.origin}.gone`, f.origin);
  assert.match(text, /\n {2}NOT PUSHED: git push failed: fatal: unable to access origin: Could not resolve host\n/);
  assert.match(text, /\n {2}OFFLINE: 1 commits unpushed, retried next run/);
});

test('credentials in a URL never reach the sync output', available, t => {
  const f = fixture(t);
  prePush(f, "echo \"fatal: unable to access 'https://ezra:ghp_TOPSECRET@github.com/o/r.git/': The requested URL returned error: 403\" >&2\nexit 1");
  f.write(f.a, 'local', 'local\n');
  const { text } = f.sync(f.a);
  assert.match(text, /NOT PUSHED: git push failed: fatal: unable to access 'https:\/\/\*\*\*@github\.com\/o\/r\.git\/': The requested URL returned error: 403/);
  assert.doesNotMatch(text, /TOPSECRET|ezra:/);
});

test('a hanging push times out and points at the credential helper', available, t => {
  const f = fixture(t); const before = f.remote();
  prePush(f, 'sleep 60');
  f.write(f.a, 'local', 'local\n');
  const started = Date.now();
  const { text } = f.sync(f.a, { pushTimeout: 500 });
  assert(Date.now() - started < 45000, 'the sync waited for the hanging hook');
  assert.match(text, /\n {2}NOT PUSHED: git push timed out after 1s; check the Git credential helper for origin, then rerun/);
  assert.doesNotMatch(text, /kept moving/);
  assert.equal(f.remote(), before);
});

test('only pushes that keep losing the race say dev kept moving', available, t => {
  const f = fixture(t);
  prePush(f, `set -eu\ndate +%s%N >> '${f.b}/raced'\ngit -C '${f.b}' add raced\ngit -C '${f.b}' commit -qm Race\ngit -C '${f.b}' push -q origin HEAD:dev`);
  f.write(f.a, 'local', 'local\n');
  const { text } = f.sync(f.a);
  assert.match(text, /\n {2}NOT PUSHED: origin\/dev kept moving; retried next run/);
  assert.doesNotMatch(text, /git push failed/);
});

test('a failed apply carries the output that explains it', available, t => {
  const f = fixture(t);
  f.write(f.b, 'remote', 'from b\n'); f.sync(f.b);
  // mise's footer (version, --verbose hint) is left out.
  process.env.DEV_TEST_APPLY_FAIL = 'mise ERROR pre-dotfiles hook failed: github:EzraCerpac/dots@0.3.1 not found\n'
    + 'mise ERROR Version: 2026.11.9 macos-arm64 (2026-11-16)\nmise ERROR Run with --verbose or MISE_VERBOSE=1 for more information';
  const { text } = f.sync(f.a);
  assert.match(text, /\n {2}APPLY FAILED: mise bootstrap dotfiles apply \(exit 1\); fix the error below, then run dots apply\n {4}applying 3 dotfiles\n {4}mise ERROR pre-dotfiles hook failed: github:EzraCerpac\/dots@0\.3\.1 not found$/);
});

test('a failed save carries the output that explains it', available, t => {
  const f = fixture(t);
  process.env.DEV_TEST_SAVE_FAIL = 'mise ERROR cannot read ~/.config/app/state.json';
  const { text } = f.sync(f.a);
  assert.match(text, /\n {2}SAVE FAILED: mise bootstrap dotfiles save \(exit 1\); tracked history not saved\n {4}mise ERROR cannot read ~\/\.config\/app\/state\.json(\n|$)/);
});

test('a push retry merges main again over a racing dev change to the same hunk', available, t => {
  const f = fixture(t);
  const c = path.join(f.dir, 'c'); git(f.dir, 'clone', '-q', f.origin, c);
  f.write(c, 'text', 'reviewed\ntwo\nthree\nfour\nfive\nsix\nseven\n'); git(c, 'commit', '-qam', 'Reviewed'); git(c, 'push', '-q', 'origin', 'main');
  const hook = path.join(f.a, '.git/hooks/pre-push');
  fs.writeFileSync(hook, `#!/bin/sh
set -eu
rm "$0"
printf 'raced\\ntwo\\nthree\\nfour\\nfive\\nsix\\nseven\\n' > '${f.b}/text'
git -C '${f.b}' commit -qam Race
git -C '${f.b}' push -q origin HEAD:dev
`);
  fs.chmodSync(hook, 0o755);
  f.write(f.a, 'local', 'local\n');
  const { text } = f.sync(f.a);
  assert.match(text, /· pushed/);
  assert.equal(git(f.origin, 'show', 'dev:text').split('\n')[0], 'reviewed');
  assert.equal(read(f.a, 'text').split('\n')[0], 'reviewed'); f.clean(f.a);
});

test('a flagged new file stays local while other edits are shared', available, t => {
  const f = fixture(t);
  f.write(f.a, 'token', 'safe line\nkey = FAKE_SECRET\n');
  f.write(f.a, 'fine', 'harmless\n');
  const { text } = f.sync(f.a);
  assert.match(text, /^dots sync: shared 1 file · .* · pushed/);
  assert.match(text, /NOT SHARED \(gitleaks fake-rule\): token:2/);
  assert.doesNotMatch(text, /FAKE_SECRET/);
  const shared = git(f.origin, 'ls-tree', '-r', '--name-only', 'dev').split('\n');
  assert(shared.includes('fine')); assert(!shared.includes('token'));
  assert.equal(git(f.a, 'status', '--porcelain'), '?? token');
  assert.equal(git(f.a, 'log', '--all', '--format=%H', '--', 'token'), '', 'no ref reaches the flagged file');
});

test('a missing scanner shares and pushes nothing', available, t => {
  const f = fixture(t); const before = f.remote();
  f.write(f.a, 'text', 'edited\n');
  const { code, text } = f.sync(f.a, { gitleaks: path.join(f.dir, 'missing-gitleaks') });
  assert.equal(code, 0);
  assert.match(text, /^dots sync: no local changes · pulled dev \+0, main \+0 · not pushed\n {2}NOT SHARED: gitleaks is not installed; local edits kept on disk, nothing pushed$/);
  assert.equal(f.remote(), before); assert.equal(read(f.a, 'text'), 'edited\n');
  assert.equal(git(f.a, 'status', '--porcelain'), 'M text');
  assert.match(f.sync(f.a).text, /shared 1 file/);
});

test('a flagged existing commit blocks the push', available, t => {
  const f = fixture(t); const before = f.remote();
  f.write(f.a, 'leak', 'FAKE_SECRET\n'); git(f.a, 'add', 'leak'); git(f.a, 'commit', '-qm', 'Oops');
  const { text } = f.sync(f.a);
  assert.match(text, /not pushed\n {2}NOT PUSHED \(gitleaks fake-rule\): commit [0-9a-f]{12} leak:1/);
  assert.equal(f.remote(), before);
});

test('an unpushed commit that ever tracked a private file blocks the push', available, t => {
  const f = fixture(t); const before = f.remote();
  f.write(f.a, 'config.host-private.toml', '[env]\nPRIVATE="yes"\n');
  git(f.a, 'add', '-f', 'config.host-private.toml'); git(f.a, 'commit', '-qm', 'Oops');
  const oops = git(f.a, 'rev-parse', 'HEAD');
  git(f.a, 'rm', '-q', '--cached', 'config.host-private.toml'); git(f.a, 'commit', '-qm', 'Untrack');
  const { text } = f.sync(f.a);
  assert.match(text, /· not pushed\n {2}NOT PUSHED: commit ([0-9a-f]{12}) changes private setup file config\.host-private\.toml; remove it from local history$/);
  assert.equal(text.match(/commit ([0-9a-f]{12})/)[1], oops.slice(0, 12));
  assert.equal(f.remote(), before);
});

test('the real gitleaks reports a token by file and line only', requires('git', 'jq', 'gitleaks'), t => {
  const f = fixture(t);
  fs.copyFileSync(new URL('../init/gitleaks.toml', import.meta.url), path.join(f.a, '.gitleaks.toml'));
  const token = ['ghp', 'R8tq3LwVn5Xk2Jc9Hy7Pd4Mb6Fz1Sa0Gu3Ee'].join('_');
  f.write(f.a, 'nested/token', `\ngithub_token = "${token}"\n`);
  const { text } = f.sync(f.a, { gitleaks: 'gitleaks' });
  assert.match(text, /NOT SHARED \(gitleaks github-pat\): nested\/token:2/);
  assert(!text.includes(token));
  assert(!git(f.origin, 'ls-tree', '-r', '--name-only', 'dev').includes('nested/token'));
});

test('an invalid merged config is neither pushed nor applied', available, t => {
  const f = fixture(t); const before = f.remote();
  f.write(f.b, 'remote', 'from b\n'); f.sync(f.b); const shared = f.remote();
  f.write(f.a, 'config.toml', 'INVALID\n');
  const { code, text } = f.sync(f.a);
  assert.equal(code, 0);
  assert.match(text, /^dots sync: saved 1 file locally · fetched dev \+1, main \+0 \(not applied\) · not pushed\n {2}INVALID merged config: .*; nothing pushed or applied$/);
  assert.notEqual(shared, before); assert.equal(f.remote(), shared);
  assert(!fs.existsSync(path.join(f.a, 'remote')));
  assert.equal(git(f.a, 'show', 'dev:config.toml'), 'INVALID'); f.clean(f.a);
  f.write(f.a, 'config.toml', '# fixed\n');
  assert.match(f.sync(f.a).text, /pushed/);
  assert.equal(read(f.a, 'remote'), 'from b\n'); assert.equal(f.remote(), f.dev(f.a));
});

test('a file edited during advancement is not overwritten', available, t => {
  const f = fixture(t);
  f.write(f.b, 'text', 'incoming remote edit\n'); f.sync(f.b);
  f.write(f.a, 'local', 'local\n');
  process.env.DEV_TEST_MUTATE = path.join(f.a, 'text');
  const { code, text } = f.sync(f.a);
  delete process.env.DEV_TEST_MUTATE;
  assert.equal(code, 0);
  assert.match(text, /fetched dev \+1, main \+0 \(not applied\) · pushed/);
  assert.match(text, /LATE EDIT: text not updated; rerun/);
  assert.equal(read(f.a, 'text'), 'late\n');
  assert.equal(git(f.origin, 'show', 'dev:local'), 'local');
  const again = f.sync(f.a);
  assert.match(again.text, /CONFLICT kept published: text/);
  assert.equal(read(f.a, 'text'), 'incoming remote edit\n'); f.clean(f.a);
});

test('a locked Git index leaves the files and dev alone', available, t => {
  const f = fixture(t); const before = f.dev(f.a);
  f.write(f.b, 'remote', 'from b\n'); f.sync(f.b);
  const lock = path.join(f.a, '.git/index.lock');
  fs.writeFileSync(lock, '');
  const { code, text } = f.sync(f.a);
  assert.equal(code, 0);
  assert.match(text, /^dots sync: no local changes · fetched dev \+1, main \+0 \(not applied\) · nothing to push\n {2}NOT APPLIED: the Git index is locked \(\.git\/index\.lock\); files not updated, rerun$/);
  assert(fs.existsSync(lock), 'the lock held by someone else stays');
  assert(!fs.existsSync(path.join(f.a, 'remote')));
  assert.equal(f.dev(f.a), before);
  fs.rmSync(lock); f.clean(f.a);
  f.sync(f.a);
  assert.equal(read(f.a, 'remote'), 'from b\n'); f.clean(f.a);
});

test('a failed index install releases the index lock and leaves dev alone', available, t => {
  const f = fixture(t); const before = f.dev(f.a);
  f.write(f.a, 'text', 'edited\n');
  const copy = fs.copyFileSync;
  fs.copyFileSync = (from, to, ...rest) => {
    if (String(to).endsWith('.install')) throw Object.assign(new Error('ENOSPC: no space left on device'), { code: 'ENOSPC' });
    return copy(from, to, ...rest);
  };
  let result;
  try { result = f.sync(f.a); } finally { fs.copyFileSync = copy; }
  assert.equal(result.code, 0);
  assert.match(result.text, /^dots sync: failed · ENOSPC/);
  assert(!fs.existsSync(path.join(f.a, '.git/index.lock')), 'the index lock is released');
  assert.equal(f.dev(f.a), before);
  assert.equal(git(f.a, 'status', '--porcelain'), 'M text');
});

test('post-checkout index failure in a linked worktree retries without publishing a snapshot', available, t => {
  const f = fixture(t), before = f.dev(f.a);
  const linked = path.join(f.dir, 'linked');
  git(f.a, 'switch', '-q', 'main');
  git(f.a, 'worktree', 'add', '-q', linked, 'dev');
  const gitDir = git(linked, 'rev-parse', '--absolute-git-dir');
  const mainIndex = fs.readFileSync(path.join(f.a, '.git/index'));
  f.write(f.b, 'text', 'incoming\n');
  f.write(f.b, 'incoming', 'new file\n');
  assert.match(f.sync(f.b).text, /pushed/);
  const published = f.remote();
  const result = failIndexInstall(linked, () => f.sync(linked), () => {
    assert.equal(read(linked, 'text'), 'incoming\n', 'inject after checkout has written');
  });
  assert.equal(result.text.match(/^dots sync: /gm).length, 1);
  assert.match(result.text, /NOT APPLIED: injected index installation failure; checkout pending, rerun sync/);
  assert.equal(f.dev(linked), before);
  assert(!fs.existsSync(path.join(gitDir, 'index.lock')));
  const lock = path.join(gitDir, 'index.lock');
  fs.writeFileSync(lock, '');
  const held = f.sync(linked);
  assert(held.text.includes(`Git index is locked (${path.relative(linked, lock)}); finish any active Git operation, or remove that lock`), held.text);
  assert(fs.existsSync(lock));
  fs.rmSync(lock);
  const retry = f.sync(linked);
  assert.match(retry.text, /^dots sync: no local changes .* nothing to push/);
  assert.equal(f.dev(linked), published);
  assert.equal(f.remote(), published);
  assert.equal(read(linked, 'incoming'), 'new file\n');
  assert.deepEqual(fs.readFileSync(path.join(f.a, '.git/index')), mainIndex);
  f.clean(linked);
});

test('a ref update failure after checkout and index install retries the same commit', available, t => {
  const f = fixture(t), before = f.dev(f.a);
  f.write(f.b, 'text', 'incoming\n');
  f.sync(f.b);
  const published = f.remote(), lock = path.join(f.a, '.git/refs/heads/dev.lock');
  fs.writeFileSync(lock, '');
  const { code, text } = f.sync(f.a);
  assert.equal(code, 0);
  assert.match(text, /NOT APPLIED: could not update dev/);
  assert.equal(f.dev(f.a), before);
  assert.equal(git(f.a, 'write-tree'), git(f.a, 'rev-parse', 'origin/dev^{tree}'));
  fs.rmSync(lock);
  assert.match(f.sync(f.a).text, /^dots sync: no local changes .* nothing to push/);
  assert.equal(f.dev(f.a), published);
  assert.equal(f.remote(), published);
  f.clean(f.a);
});

test('a partial checkout retries without publishing incoming files as local edits', available, t => {
  const f = fixture(t), before = f.dev(f.a);
  f.write(f.b, '.gitattributes', 'z-filtered filter=fixture\n');
  f.write(f.b, ' a-first', 'incoming first\n');
  f.write(f.b, 'z-filtered', 'incoming last\n');
  f.sync(f.b);
  const published = f.remote();
  git(f.a, 'config', 'filter.fixture.required', 'true');
  git(f.a, 'config', 'filter.fixture.clean', 'cat');
  // The incoming preview also runs smudge through git archive. Fail only
  // while checkout is running against sync's temporary index.
  git(f.a, 'config', 'filter.fixture.smudge', 'case "$GIT_INDEX_FILE" in */dots-index-*/index) exit 1 ;; *) cat ;; esac');
  const { code, text } = f.sync(f.a);
  assert.equal(code, 0);
  assert.match(text, /NOT APPLIED: .*checkout pending/);
  assert.equal(read(f.a, ' a-first'), 'incoming first\n');
  assert(!fs.existsSync(path.join(f.a, 'z-filtered')));
  assert.equal(f.dev(f.a), before);
  git(f.a, 'config', 'filter.fixture.smudge', 'cat');
  assert.match(f.sync(f.a).text, /^dots sync: no local changes .* nothing to push/);
  assert.equal(f.dev(f.a), published);
  assert.equal(f.remote(), published);
  assert.equal(read(f.a, 'z-filtered'), 'incoming last\n');
  f.clean(f.a);
});

test('incoming tracking preserves ignored obstructions while unrelated edits are shared', available, async t => {
  for (const shape of ['file', 'directory', 'symlink parent', 'ignored child']) {
    await t.test(shape, t => {
      const f = fixture(t);
      const incoming = shape === 'symlink parent' ? 'collision/item' : 'collision';
      const blocked = shape === 'ignored child' ? 'collision/private' : 'collision';
      if (shape === 'ignored child') {
        f.write(f.b, 'collision/public', 'tracked child\n'); f.sync(f.b); f.sync(f.a);
        fs.rmSync(path.join(f.b, 'collision'), { recursive: true });
      }
      f.write(f.a, '.git/info/exclude', `/${blocked}\n`);
      if (shape === 'symlink parent') fs.symlinkSync('missing', path.join(f.a, blocked));
      else if (shape === 'directory') fs.mkdirSync(path.join(f.a, blocked));
      else f.write(f.a, blocked, 'local only\n');
      f.write(f.b, incoming, 'incoming\n');
      git(f.b, 'add', '-A'); git(f.b, 'commit', '-qm', 'Start tracking'); git(f.b, 'push', '-q', 'origin', 'dev');
      f.write(f.a, 'unrelated', 'share this\n');
      const { code, text } = f.sync(f.a);
      assert.equal(code, 0);
      assert(text.includes(`BLOCKED PATH: ${blocked} obstructs an incoming tracked path; move it aside, rerun sync`), text);
      assert.equal(git(f.origin, 'show', 'dev:unrelated'), 'share this');
      if (shape === 'symlink parent') assert.equal(fs.readlinkSync(path.join(f.a, blocked)), 'missing');
      else if (shape === 'directory') assert(fs.lstatSync(path.join(f.a, blocked)).isDirectory());
      else assert.equal(read(f.a, blocked), 'local only\n');
      fs.renameSync(path.join(f.a, blocked), path.join(f.dir, 'kept'));
      assert.match(f.sync(f.a).text, /pulled/);
      assert.equal(read(f.a, incoming), 'incoming\n');
      f.clean(f.a);
    });
  }
});

test('a case-only tracked rename can advance', available, t => {
  const f = fixture(t);
  // An intermediate name avoids Git case-folding the entry when staging.
  git(f.b, 'mv', 'text', 'intermediate');
  git(f.b, 'mv', 'intermediate', 'Text');
  git(f.b, 'commit', '-qm', 'Change case');
  git(f.b, 'push', '-q', 'origin', 'dev');
  const { code, text } = f.sync(f.a);
  assert.equal(code, 0);
  assert.match(text, /pulled dev \+1/);
  assert.doesNotMatch(text, /BLOCKED PATH/);
  assert.deepEqual(fs.readdirSync(f.a).filter(name => name.toLowerCase() === 'text'), ['Text']);
  assert.equal(f.dev(f.a), f.remote());
  f.clean(f.a);
});

test('a case-only rename retries without publishing the incoming path as a deletion', available, t => {
  const f = fixture(t);
  const original = read(f.b, 'text');
  for (const [from, to, expected] of [['text', 'Text', original], ['Text', 'text', 'incoming new contents\n']]) {
    git(f.b, 'mv', from, 'intermediate');
    f.write(f.b, 'intermediate', expected);
    git(f.b, 'mv', 'intermediate', to);
    git(f.b, 'commit', '-qam', 'Change case');
    git(f.b, 'push', '-q', 'origin', 'dev');
    const published = f.remote();
    failIndexInstall(f.a, () => f.sync(f.a));
    assert.match(f.sync(f.a).text, /^dots sync: no local changes .* nothing to push/);
    assert.equal(f.remote(), published);
    assert.equal(f.dev(f.a), published);
    assert.equal(read(f.a, to), expected);
    f.clean(f.a);
  }
});

test('local edits are not reported saved when dev or the index cannot be recorded', available, t => {
  const f = fixture(t); const before = f.dev(f.a);
  f.write(f.a, 'config.toml', 'INVALID\n');
  for (const name of ['refs/heads/dev.lock', 'index.lock']) {
    const lock = path.join(f.a, '.git', name);
    fs.writeFileSync(lock, '');
    const { code, text } = f.sync(f.a);
    fs.rmSync(lock);
    assert.equal(code, 0);
    assert.match(text, /^dots sync: no local changes · [^\n]*\n {2}NOT SAVED: [^\n]*; local edits kept on disk, rerun$/);
    assert.equal(f.dev(f.a), before, name);
    assert.equal(git(f.a, 'status', '--porcelain'), 'M config.toml', `${name}: the original index stays`);
  }
});

test('a commit pushed to origin/dev from another worktree is merged, not reverted', available, t => {
  const f = fixture(t);
  const wt = path.join(f.dir, 'task');
  git(f.a, 'worktree', 'add', '-q', '--detach', wt, 'origin/dev');
  f.write(wt, 'task-file', 'from the task worktree\n');
  f.write(wt, 'text', 'one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n');
  git(wt, 'add', '.'); git(wt, 'commit', '-qm', 'Task change'); git(wt, 'push', '-q', 'origin', 'HEAD:dev');
  const task = git(wt, 'rev-parse', 'HEAD');
  f.write(f.a, 'text', 'ONE\ntwo\nthree\nfour\nfive\nsix\nseven\n');
  const { text } = f.sync(f.a);
  assert.match(text, /pulled dev \+1, main \+0 · pushed/);
  git(f.origin, 'merge-base', '--is-ancestor', task, 'dev');
  assert.equal(git(f.origin, 'show', 'dev:task-file'), 'from the task worktree');
  assert.equal(git(f.origin, 'show', 'dev:text'), 'ONE\ntwo\nthree\nfour\nfive\nsix\nseven\neight');
  assert.equal(git(f.origin, 'diff', '--name-only', task, 'dev'), 'text', 'only the local edit differs from the task commit');
  f.clean(f.a);
});

test('another worktree cannot quietly move dev under the live files', available, t => {
  const f = fixture(t);
  const wt = path.join(f.dir, 'task');
  git(f.a, 'worktree', 'add', '-q', '--detach', wt, 'dev');
  f.write(wt, 'moved', 'dev moved\n'); git(wt, 'add', '.'); git(wt, 'commit', '-qm', 'Move dev');
  const moved = git(wt, 'rev-parse', 'HEAD');
  git(wt, 'push', '-q', 'origin', 'HEAD:refs/heads/elsewhere');
  assert.notEqual(spawnSync('git', ['fetch', '-q', 'origin', 'elsewhere:dev'], { cwd: wt }).status, 0, 'fetch into the checked-out dev is refused');
  assert.notEqual(spawnSync('git', ['branch', '-f', 'dev', moved], { cwd: wt }).status, 0, 'branch -f on the checked-out dev is refused');
  git(wt, 'update-ref', 'refs/heads/dev', moved); // plumbing still can
  f.write(f.a, 'text', 'local edit\n');
  const { text } = f.sync(f.a);
  assert.match(text, /dev moved outside sync/);
  assert.equal(git(f.origin, 'show', 'dev:moved'), 'dev moved');
  assert.equal(git(f.origin, 'show', 'dev:text'), 'local edit');
  assert.equal(read(f.a, 'moved'), 'dev moved\n'); f.clean(f.a);
});

test('files follow dev when it moved outside sync and nothing changed locally', available, t => {
  const f = fixture(t);
  const wt = path.join(f.dir, 'task');
  git(f.a, 'worktree', 'add', '-q', '--detach', wt, 'dev');
  f.write(wt, 'moved', 'dev moved\n'); git(wt, 'add', '.'); git(wt, 'commit', '-qm', 'Move dev');
  git(wt, 'update-ref', 'refs/heads/dev', git(wt, 'rev-parse', 'HEAD'));
  const { text } = f.sync(f.a);
  assert.match(text, /dev moved outside sync/);
  assert.equal(read(f.a, 'moved'), 'dev moved\n'); f.clean(f.a);
});

test('an index staged to match an older commit loses no file content', available, t => {
  const f = fixture(t);
  f.write(f.a, 'text', 'changed\n'); f.sync(f.a);
  git(f.a, 'checkout', 'HEAD~1', '--', 'text'); // hand-staged revert: index tree equals HEAD~1's
  f.write(f.a, 'other', 'unstaged edit\n');
  const { text } = f.sync(f.a);
  // The staged revert reads as a stale base: it is reported and undone, but every file on disk is shared.
  assert.match(text, /the index was staged to match [0-9a-f]{12}; local files were merged onto dev, not reverted/);
  assert.equal(git(f.origin, 'show', 'dev:other'), 'unstaged edit');
  assert.equal(read(f.a, 'text'), 'changed\n'); f.clean(f.a);
});

test('a checkout in the wrong shape stops with one message pointing at the sync guide', available, t => {
  const f = fixture(t);
  const before = f.remote();
  const expect = (problem, prepare, undo) => {
    prepare();
    const { code, text } = f.sync(f.a);
    assert.equal(code, 0);
    assert.equal(text.split('\n').length, 1);
    assert.match(text, new RegExp(`^dots sync: stopped · ${problem}\\. .*/docs/sync\\.md$`));
    undo();
  };
  expect('HEAD is detached, not on dev', () => git(f.a, 'switch', '-q', '--detach'), () => git(f.a, 'switch', '-q', 'dev'));
  expect('HEAD is on main, not dev', () => git(f.a, 'switch', '-q', 'main'), () => git(f.a, 'switch', '-q', 'dev'));
  expect('a JJ workspace \\(\\.jj\\) is present', () => fs.mkdirSync(path.join(f.a, '.jj')), () => fs.rmdirSync(path.join(f.a, '.jj')));
  expect('legacy sync state \\(\\.git/dots-dev\\.json\\) is present', () => f.write(f.a, '.git/dots-dev.json', '{}'), () => fs.rmSync(path.join(f.a, '.git/dots-dev.json')));
  assert.equal(f.remote(), before);
  const foreign = [];
  assert.equal(syncSource(f.a, { expectedRemote: '/elsewhere', print: text => foreign.push(text) }), 0);
  assert.deepEqual(foreign, ['dots sync: stopped · Refusing source operations: origin is not /elsewhere']);
});

test('an unexpected error prints one failed line and exits 0', available, t => {
  const f = fixture(t); const before = f.dev(f.a);
  fs.writeFileSync(process.env.GIT_CONFIG_GLOBAL, '[user]\nuseConfigOnly = true\n');
  for (const key of ['EMAIL', 'GIT_AUTHOR_NAME', 'GIT_AUTHOR_EMAIL', 'GIT_COMMITTER_NAME', 'GIT_COMMITTER_EMAIL']) delete process.env[key];
  f.write(f.a, 'text', 'needs a commit\n');
  const { code, text } = f.sync(f.a);
  assert.equal(code, 0);
  assert.match(text, /^dots sync: failed · git [^\n]* failed: Author identity unknown$/);
  assert.equal(f.dev(f.a), before); assert.equal(read(f.a, 'text'), 'needs a commit\n');
});

test('a concurrent run is skipped and a dead run\'s lock is reclaimed', available, t => {
  const f = fixture(t);
  const lock = path.join(f.a, '.git/dots-source.lock');
  fs.mkdirSync(lock);
  fs.writeFileSync(path.join(lock, 'owner.json'), JSON.stringify({ pid: process.pid, host: os.hostname() }));
  f.write(f.a, 'text', 'waiting\n');
  const busy = f.sync(f.a);
  assert.equal(busy.code, 0);
  assert.match(busy.text, /^dots sync: skipped · another source operation is running \(pid \d+\)/);
  assert.equal(git(f.a, 'status', '--porcelain'), 'M text');
  const dead = spawnSync(process.execPath, ['-e', 'process.stdout.write(String(process.pid))'], { encoding: 'utf8' }).stdout;
  fs.writeFileSync(path.join(lock, 'owner.json'), JSON.stringify({ pid: Number(dead), host: os.hostname() }));
  assert.match(f.sync(f.a).text, /shared 1 file/);
  assert(!fs.existsSync(lock));
});

test('a stale lock is reclaimed by one process only and each run releases only its own lock', available, t => {
  const f = fixture(t);
  const lock = path.join(f.a, '.git/dots-source.lock'), owner = path.join(lock, 'owner.json');
  const dead = Number(spawnSync(process.execPath, ['-e', 'process.stdout.write(String(process.pid))'], { encoding: 'utf8' }).stdout);
  fs.mkdirSync(lock);
  fs.writeFileSync(owner, JSON.stringify({ pid: dead, host: os.hostname() }));
  // Another run reclaims the stale lock right after this one read its dead owner.
  const live = JSON.stringify({ pid: process.pid, host: os.hostname(), started: 'other run' });
  const readFile = fs.readFileSync;
  let raced = false;
  fs.readFileSync = (file, ...rest) => {
    const text = readFile(file, ...rest);
    if (!raced && file === owner) { raced = true; fs.rmSync(lock, { recursive: true }); fs.mkdirSync(lock); fs.writeFileSync(owner, live); }
    return text;
  };
  let ran = false, error;
  try { withRepositoryLock(f.a, () => { ran = true; }); } catch (caught) { error = caught; } finally { fs.readFileSync = readFile; }
  assert(!ran, 'both runs held the lock');
  assert(error?.locked, String(error));
  assert.equal(fs.readFileSync(owner, 'utf8'), live, 'the other run keeps its lock');
  // A run whose lock was taken over leaves the new holder's lock alone.
  fs.rmSync(lock, { recursive: true });
  withRepositoryLock(f.a, () => fs.writeFileSync(owner, live));
  assert.equal(fs.readFileSync(owner, 'utf8'), live);
});

test('a retired local declaration goes once the commit that retires it is on disk', available, t => {
  const f = fixture(t); const local = olderHost(f);
  f.write(f.a, 'config.host-laptop.toml', '[env]\nHOST = "1"\n');
  const { code, text } = f.sync(f.a);
  assert.equal(code, 0);
  assert.equal(text.match(/^dots sync: /gm).length, 1);
  assert.match(text, /^dots sync: no local changes · pulled dev \+1, main \+0 · nothing to push\n {2}changed on disk: 2 files changed/);
  const backup = text.match(RETIRED('config.local.toml'))?.[1];
  assert(backup, text);
  assert.equal(read(f.a, 'config.local.toml'), KEPT);
  assert.equal(fs.statSync(path.join(f.a, 'config.local.toml')).mode & 0o777, 0o600);
  assert.equal(read(f.a, backup), local);
  assert.equal(fs.statSync(path.join(f.a, backup)).mode & 0o777, 0o600);
  assert.equal(read(f.a, 'config.host-laptop.toml'), '[env]\nHOST = "1"\n');
  assert(!fs.existsSync(path.join(f.a, 'templates/herdr.tera')));
  f.clean(f.a);
  const again = f.sync(f.a);
  assert.equal(again.text, 'dots sync: no local changes · pulled dev +0, main +0 · nothing to push');
  assert.equal(read(f.a, 'config.local.toml'), KEPT);
});

test('without a retired-declarations entry the declaration still fails validation', available, t => {
  const f = fixture(t); const local = olderHost(f, { listed: false });
  const stat = fs.statSync(path.join(f.a, 'config.local.toml'));
  const { code, text } = f.sync(f.a);
  assert.equal(code, 0);
  assert.match(text, /^dots sync: no local changes · fetched dev \+1, main \+0 \(not applied\) · not pushed\n {2}INVALID merged config: [^\n]*sources do not exist: templates\/herdr\.tera; nothing pushed or applied$/);
  unchangedLocal(f, 'config.local.toml', local, stat);
  assert(fs.existsSync(path.join(f.a, 'templates/herdr.tera')));
});

test('a declaration the user pointed elsewhere is not retired', available, t => {
  const f = fixture(t); const local = olderHost(f, { local: KEPT + HERDR.replace('templates/herdr.tera', 'text') });
  const stat = fs.statSync(path.join(f.a, 'config.local.toml'));
  const { text } = f.sync(f.a);
  assert.match(text, /^dots sync: no local changes · pulled dev \+1, main \+0 · nothing to push\n {2}changed on disk: [^\n]*$/);
  unchangedLocal(f, 'config.local.toml', local, stat);
});

test('a retired declaration stays while the merged config is invalid for another reason', available, t => {
  const f = fixture(t); const local = olderHost(f);
  const stat = fs.statSync(path.join(f.a, 'config.local.toml'));
  f.write(f.a, 'config.toml', 'INVALID\n');
  const { code, text } = f.sync(f.a);
  assert.equal(code, 0);
  assert.match(text, /\n {2}INVALID merged config: [^\n]*invalid config; nothing pushed or applied$/);
  assert.doesNotMatch(text, /RETIRED/);
  unchangedLocal(f, 'config.local.toml', local, stat);
  assert(fs.existsSync(path.join(f.a, 'templates/herdr.tera')));
});

test('a retired declaration stays until the files advance', available, t => {
  const f = fixture(t); const local = olderHost(f, { file: 'config.host-laptop.toml' });
  const stat = fs.statSync(path.join(f.a, 'config.host-laptop.toml'));
  const lock = path.join(f.a, '.git/index.lock');
  fs.writeFileSync(lock, '');
  const { code, text } = f.sync(f.a);
  assert.equal(code, 0);
  assert.match(text, /^dots sync: no local changes · fetched dev \+1, main \+0 \(not applied\) · nothing to push\n {2}NOT APPLIED: [^\n]*$/);
  unchangedLocal(f, 'config.host-laptop.toml', local, stat);
  assert(fs.existsSync(path.join(f.a, 'templates/herdr.tera')));
  fs.rmSync(lock);
  assert.match(f.sync(f.a).text, RETIRED('config.host-laptop.toml'));
  assert.equal(read(f.a, 'config.host-laptop.toml'), KEPT);
});

// A sub-table that makes the declaration unretirable, as an editor would save it.
const VARIANT = '\n[[dotfiles.herdr.variants]]\nos = "linux"\n';
const INVALID_HERDR = /\n {2}INVALID merged config: [^\n]*sources do not exist: templates\/herdr\.tera; nothing pushed or applied$/;

test('a local file edited while it is validated holds the files back', available, t => {
  const f = fixture(t); const local = olderHost(f);
  const marker = path.join(f.dir, 'edited'), miseBin = path.join(f.dir, 'mise-editing');
  fs.writeFileSync(miseBin, `#!/bin/sh
case "$*" in *'config ls'*) [ -e '${marker}' ] || { : > '${marker}'; printf '%s' '${VARIANT}' >> '${path.join(f.a, 'config.local.toml')}'; } ;; esac
exec '${path.join(f.dir, 'mise')}' "$@"
`);
  fs.chmodSync(miseBin, 0o755);
  const { code, text } = f.sync(f.a, { miseBin });
  assert.equal(code, 0);
  assert.equal(text, 'dots sync: no local changes · fetched dev +1, main +0 (not applied) · nothing to push\n  NOT APPLIED: config.local.toml changed during sync; files not updated, rerun');
  assert.equal(read(f.a, 'config.local.toml'), local + VARIANT);
  assert(fs.existsSync(path.join(f.a, 'templates/herdr.tera')), 'the tree did not advance');
  assert(!fs.existsSync(path.join(f.a, '.git/dots-retired')));
  // The next run validates the edited file, which now needs a hand.
  assert.match(f.sync(f.a).text, INVALID_HERDR);
  assert(fs.existsSync(path.join(f.a, 'templates/herdr.tera')));
  f.write(f.a, 'config.local.toml', local);
  assert.match(f.sync(f.a).text, RETIRED('config.local.toml'));
  assert.equal(read(f.a, 'config.local.toml'), KEPT);
});

test('a local file edited once the files advance is reported, not silently kept', available, t => {
  const f = fixture(t); const local = olderHost(f);
  // Git runs this hook as sync moves dev to the merged commit, after the checkout.
  const hook = path.join(f.a, '.git/hooks/reference-transaction');
  fs.writeFileSync(hook, `#!/bin/sh
[ "$1" = committed ] && grep -q ' refs/heads/dev$' && printf '%s' '${VARIANT}' >> '${path.join(f.a, 'config.local.toml')}'
exit 0
`);
  fs.chmodSync(hook, 0o755);
  const { code, text } = f.sync(f.a);
  fs.rmSync(hook);
  assert.equal(code, 0);
  assert.equal(text.match(/^dots sync: /gm).length, 1);
  assert.match(text, /^dots sync: no local changes · pulled dev \+1, main \+0 · nothing to push\n/);
  assert.match(text, /\n {2}NOT RETIRED: local declaration \[dotfiles\.herdr\] in config\.local\.toml, which changed during sync; rerun, or remove the table by hand\n/);
  assert.doesNotMatch(text, /RETIRED local/);
  assert.equal(read(f.a, 'config.local.toml'), local + VARIANT);
  assert(!fs.existsSync(path.join(f.a, '.git/dots-retired')));
  assert.match(f.sync(f.a).text, INVALID_HERDR);
});

function copiedFixture(t, { hooks = false } = {}) {
  const f = fixture(t), miseBin = miseBinary();
  for (const key of Object.keys(process.env)) {
    if (key.startsWith('MISE_') || key.startsWith('__MISE_')) delete process.env[key];
  }
  const machine = (root, name) => {
    const local = path.join(f.dir, name), home = path.join(local, 'home'), system = path.join(local, 'system');
    fs.mkdirSync(home, { recursive: true }); fs.mkdirSync(system);
    const env = { HOME: home, MISE_CONFIG_DIR: root, MISE_DATA_DIR: path.join(local, 'data'), MISE_STATE_DIR: path.join(local, 'state'),
      MISE_CACHE_DIR: path.join(local, 'cache'), MISE_SYSTEM_CONFIG_DIR: system, MISE_TRUSTED_CONFIG_PATHS: root,
      MISE_CEILING_PATHS: fs.realpathSync(os.tmpdir()), MISE_AUTO_INSTALL: '0', MISE_NO_HOOKS: hooks ? '0' : '1', MISE_AUTO_UPDATE: '0' };
    return {
      home, env,
      sync: (options = {}) => { Object.assign(process.env, env); return f.sync(root, { miseBin, ...options }); },
      deploy: () => run(miseBin, ['-C', root, 'bootstrap', 'dotfiles', 'apply', '--yes'], root, { env: { ...process.env, ...env } }),
    };
  };
  const a = machine(f.a, 'machine-a'), b = machine(f.b, 'machine-b');
  f.write(f.a, 'config.toml', '[dotfiles]\n"~/.copied" = { source = "copy.txt", mode = "copy" }\n'
    + '"~/.linked" = { source = "link.txt", mode = "symlink" }\n'
    + '"~/.rendered" = { source = "template.tera", mode = "template" }\n');
  f.write(f.a, 'copy.txt', 'initial\n');
  f.write(f.a, 'link.txt', 'linked\n');
  f.write(f.a, 'template.tera', '{{ "rendered" }}\n');
  for (const m of [a, b]) {
    const result = m.sync();
    assert.equal(result.code, 0);
    assert(fs.existsSync(path.join(m.home, '.copied')), result.text);
    assert.equal(read(m.home, '.copied'), 'initial\n', result.text);
  }
  return { ...f, machineA: a, machineB: b, miseBin };
}

test('real mise captures a second edit while an incoming checkout stays blocked', requires('git', 'jq', 'mise'), t => {
  const f = copiedFixture(t), a = f.machineA;
  f.write(f.a, '.git/info/exclude', '/obstruction\n');
  f.write(f.a, 'obstruction', 'private obstruction\n');
  f.write(f.b, 'obstruction', 'incoming tracked file\n');
  git(f.b, 'add', 'obstruction');
  git(f.b, 'commit', '-qm', 'Track obstruction');
  git(f.b, 'push', '-q', 'origin', 'dev');
  for (const edit of ['first captured edit\n', 'second captured edit\n']) {
    f.write(a.home, '.copied', edit);
    const result = a.sync();
    assert.equal(result.code, 0);
    assert.match(result.text, /BLOCKED PATH: obstruction/);
    assert.equal(read(f.a, 'copy.txt'), edit, result.text);
    assert.equal(git(f.origin, 'show', 'dev:copy.txt'), edit.trim());
    assert.equal(read(f.a, 'obstruction'), 'private obstruction\n');
  }
});

test('real mise leaves the source alone when it cannot record a capture', requires('git', 'jq', 'mise'), t => {
  const f = copiedFixture(t), a = f.machineA;
  const target = path.join(a.home, '.copied'), records = path.join(f.a, '.git/dots-copied.json');
  f.write(a.home, '.copied', 'edit to keep\n');
  f.write(f.a, 'unrelated', 'still share this\n');
  const rename = fs.renameSync;
  fs.renameSync = (from, to) => {
    if (to === records) throw new Error('injected record failure');
    return rename(from, to);
  };
  let result;
  try { result = a.sync(); } finally { fs.renameSync = rename; }
  assert.match(result.text, /CAPTURE FAILED: .*injected record failure/);
  assert.equal(read(f.a, 'copy.txt'), 'initial\n');
  assert.equal(fs.readFileSync(target, 'utf8'), 'edit to keep\n');
  assert.equal(git(f.origin, 'show', 'dev:unrelated'), 'still share this');
  assert.match(a.sync().text, /shared 1 file/);
  assert.equal(git(f.origin, 'show', 'dev:copy.txt'), 'edit to keep');
});

test('real mise retries capture after stopping between the record and source writes', requires('git', 'jq', 'mise'), t => {
  const f = copiedFixture(t), a = f.machineA;
  f.write(a.home, '.copied', 'interrupted copy edit\n');
  const program = `import fs from 'node:fs';
import path from 'node:path';
import { syncSource } from ${JSON.stringify(new URL('../lib/source-repo.mjs', import.meta.url).href)};
const [root, expectedRemote, miseBin, gitleaks] = process.argv.slice(1);
const rename = fs.renameSync;
fs.renameSync = (from, to) => {
  if (to === path.join(root, 'copy.txt')) process.exit(73);
  return rename(from, to);
};
process.exit(syncSource(root, { expectedRemote, miseBin, gitleaks }));
`;
  const stopped = spawnSync(process.execPath, ['--input-type=module', '-e', program, f.a, f.origin, f.miseBin, f.gitleaks],
    { env: { ...process.env, ...a.env }, encoding: 'utf8' });
  assert.equal(stopped.status, 73, stopped.stderr);
  assert.equal(read(f.a, 'copy.txt'), 'initial\n');
  assert.match(a.sync().text, /shared 1 file/);
  assert.equal(git(f.origin, 'show', 'dev:copy.txt'), 'interrupted copy edit');
  assert.doesNotMatch(git(f.origin, 'ls-tree', '-r', '--name-only', 'dev'), /\.dots-write-/);
  f.clean(f.a);
});

test('real mise preserves a copy edited after inspection and before apply', requires('git', 'jq', 'mise'), t => {
  const f = copiedFixture(t), a = f.machineA, b = f.machineB;
  f.write(f.b, 'copy.txt', 'incoming version\n');
  f.write(f.b, 'template.tera', '{{ "incoming template" }}\n');
  assert.match(b.sync().text, /shared 2 files/);
  const wrapper = path.join(f.dir, 'mise-inject');
  const marker = path.join(f.dir, 'injected');
  const target = path.join(a.home, '.copied');
  fs.writeFileSync(wrapper, `#!/bin/sh
set -eu
if [ "\${3:-}" = bootstrap ] && [ "\${4:-}" = dotfiles ] && [ "\${5:-}" = apply ]; then
  case " $* " in
    *" --dry-run "*) ;;
    *) printf 'late edit\\n' > '${target}.edit'; mv '${target}.edit' '${target}'; touch '${marker}' ;;
  esac
fi
exec '${f.miseBin}' "$@"
`, { mode: 0o755 });
  const { code, text } = a.sync({ miseBin: wrapper });
  assert.equal(code, 0);
  assert(fs.existsSync(marker), 'the edit was injected at apply');
  assert.equal(read(f.a, 'copy.txt'), 'incoming version\n');
  assert.equal(read(a.home, '.rendered'), 'incoming template\n');
  assert.equal(read(a.home, '.copied'), 'late edit\n', text);
  assert.match(text, /COPY KEPT: .*\.copied/);
});

test('real mise sync reports the recovery path if a writer recreates the copy during installation', requires('git', 'jq', 'mise'), t => {
  const f = copiedFixture(t), a = f.machineA, b = f.machineB;
  const target = path.join(a.home, '.copied');
  f.write(f.b, 'copy.txt', 'incoming version\n');
  assert.match(b.sync().text, /shared 1 file/);
  const rename = fs.renameSync, link = fs.linkSync;
  fs.renameSync = (from, to) => {
    if (from === target) fs.writeFileSync(target, 'edit just before the move\n');
    return rename(from, to);
  };
  fs.linkSync = (from, to) => {
    if (to === target && path.basename(from) === 'previous') fs.writeFileSync(target, 'recreated file\n', { flag: 'wx' });
    return link(from, to);
  };
  let result;
  try { result = a.sync(); }
  finally { fs.renameSync = rename; fs.linkSync = link; }
  assert.equal(result.code, 0);
  const kept = result.text.match(/COPY KEPT: ~\/\.copied: copy changed before installation; recover the previous file from (.*); reconcile it with/);
  assert(kept, result.text);
  assert.equal(read(a.home, '.copied'), 'recreated file\n');
  assert.equal(read(f.a, 'copy.txt'), 'incoming version\n');
  assert.equal(fs.readFileSync(kept[1], 'utf8'), 'edit just before the move\n');
  assert.equal(fs.statSync(path.dirname(kept[1])).mode & 0o777, 0o700);
});

test('real mise sync preserves edits through an already open copy descriptor', requires('git', 'jq', 'mise'), t => {
  const f = copiedFixture(t), a = f.machineA, b = f.machineB;
  const target = path.join(a.home, '.copied');
  f.write(f.b, 'copy.txt', 'incoming version\n');
  assert.match(b.sync().text, /shared 1 file/);
  const fd = fs.openSync(target, 'r+'), link = fs.linkSync;
  fs.linkSync = (from, to) => {
    if (to === target && path.basename(from) === 'incoming') {
      fs.ftruncateSync(fd, 0);
      fs.writeSync(fd, 'edit through the open descriptor\n');
    }
    return link(from, to);
  };
  let result;
  try { result = a.sync(); } finally { fs.linkSync = link; fs.closeSync(fd); }
  const kept = result.text.match(/COPY KEPT: ~\/\.copied: the displaced copy was edited during installation; recover the previous file from (.*); reconcile it with/);
  assert(kept, result.text);
  assert.equal(fs.readFileSync(target, 'utf8'), 'incoming version\n');
  assert.equal(fs.readFileSync(kept[1], 'utf8'), 'edit through the open descriptor\n');
});

test('real mise sync preserves a late target when the copy source is a symlink', requires('git', 'jq', 'mise'), t => {
  const f = copiedFixture(t), a = f.machineA, b = f.machineB;
  const target = path.join(a.home, '.copied');
  fs.rmSync(target);
  fs.renameSync(path.join(f.b, 'copy.txt'), path.join(f.b, 'contents.txt'));
  fs.symlinkSync('contents.txt', path.join(f.b, 'copy.txt'));
  f.write(f.b, 'contents.txt', 'incoming symlink source\n');
  assert.match(b.sync().text, /pushed/);
  const link = fs.linkSync;
  fs.linkSync = (from, to) => {
    if (to === target && path.basename(from) === 'incoming') fs.writeFileSync(target, 'late new target\n', { flag: 'wx' });
    return link(from, to);
  };
  let result;
  try { result = a.sync(); } finally { fs.linkSync = link; }
  assert.match(result.text, /COPY KEPT: ~\/\.copied: another file claimed the target/);
  assert.equal(fs.readFileSync(target, 'utf8'), 'late new target\n');
  assert.equal(read(f.a, 'copy.txt'), 'incoming symlink source\n');
});

test('guarded copies run between real mise pre and post hooks', requires('git', 'jq', 'mise'), async t => {
  for (const onlyCopy of [false, true]) {
    await t.test(onlyCopy ? 'copy only' : 'mixed targets', t => {
      const f = copiedFixture(t, { hooks: true }), a = f.machineA;
      const before = path.join(f.dir, 'pre'), after = path.join(f.dir, 'post');
      const fail = path.join(f.dir, 'fail-pre'), edit = path.join(f.dir, 'edit-in-pre');
      const config = read(f.a, 'config.toml');
      const declarations = onlyCopy ? config.split('\n').slice(0, 2).join('\n') + '\n' : config;
      f.write(f.a, 'config.toml', declarations + '[bootstrap.hooks]\n'
        + `pre-dotfiles = ${JSON.stringify(`cat "$HOME/.copied" > '${before}'; if test -e '${edit}'; then echo "late native hook edit" > "$HOME/.copied"; fi; test ! -e '${fail}'`)}\n`
        + `post-dotfiles = ${JSON.stringify(`cat "$HOME/.copied" > '${after}'`)}\n`);
      f.write(f.a, 'copy.txt', 'hook-visible copy\n');
      fs.writeFileSync(fail, '');
      assert.match(a.sync().text, /APPLY FAILED/);
      assert.equal(read(a.home, '.copied'), 'initial\n');
      assert(!fs.existsSync(after), 'post hook must not run after pre fails');
      fs.rmSync(fail);
      const result = a.sync();
      assert.doesNotMatch(result.text, /APPLY FAILED|COPY KEPT/);
      assert.equal(fs.readFileSync(before, 'utf8'), 'initial\n');
      assert.equal(fs.readFileSync(after, 'utf8'), 'hook-visible copy\n');
      assert.equal(read(a.home, '.copied'), 'hook-visible copy\n');
      f.write(f.a, 'copy.txt', 'next source version\n');
      fs.writeFileSync(edit, '');
      assert.match(a.sync().text, /COPY KEPT: ~\/\.copied: copy changed before installation/);
      assert.equal(read(a.home, '.copied'), 'late native hook edit\n');
      assert.equal(fs.readFileSync(after, 'utf8'), 'late native hook edit\n');
      assert.equal(read(f.a, 'copy.txt'), 'next source version\n');
      assert(!fs.readdirSync(f.a).some(name => name.startsWith('config.host-dots-sync-')));
      f.clean(f.a);
    });
  }
});

test('real mise sync installs hook-generated copy contents in the same run', requires('git', 'jq', 'mise'), t => {
  const f = copiedFixture(t, { hooks: true }), a = f.machineA;
  let config = read(f.a, 'config.toml')
    + '"~/.pending-copy" = { source = "pending.txt", mode = "copy" }\n'
    + '"~/.kept-copy" = { source = "kept.txt", mode = "copy", create_if_missing = true }\n';
  f.write(f.a, 'pending.txt', 'initial\n');
  f.write(f.a, 'kept.txt', 'initial\n');
  f.write(f.a, 'config.toml', config);
  assert.doesNotMatch(a.sync().text, /APPLY FAILED|COPY KEPT/);

  const generated = path.join(f.dir, 'generated'), observation = path.join(f.dir, 'post-copy-contents');
  fs.writeFileSync(generated, 'generated by pre-hook\n');
  const pre = `for source in copy.txt pending.txt kept.txt; do cat '${generated}' > "$source"; done`;
  const post = `cat "$HOME/.copied" "$HOME/.pending-copy" "$HOME/.kept-copy" > '${observation}'`;
  config += `[bootstrap.hooks]\npre-dotfiles = ${JSON.stringify(pre)}\npost-dotfiles = ${JSON.stringify(post)}\n`;
  f.write(f.a, 'config.toml', config);
  f.write(f.a, 'pending.txt', 'planned before pre-hook\n');
  const result = a.sync();
  assert.equal(result.code, 0);
  assert.doesNotMatch(result.text, /APPLY FAILED|COPY KEPT/);
  assert.equal(fs.readFileSync(observation, 'utf8'), 'generated by pre-hook\ngenerated by pre-hook\ninitial\n');

  const target = path.join(a.home, '.copied');
  const before = fs.statSync(target, { bigint: true });
  const unchanged = a.sync();
  assert.doesNotMatch(unchanged.text, /previous copy of|APPLY FAILED|COPY KEPT/);
  const after = fs.statSync(target, { bigint: true });
  assert.equal(after.ino, before.ino);
  assert.equal(after.mtimeNs, before.mtimeNs);
  assert.equal(fs.readFileSync(observation, 'utf8'), 'generated by pre-hook\ngenerated by pre-hook\ninitial\n');

  f.write(a.home, '.kept-copy', 'customized on this host\n');
  const customized = a.sync();
  assert.match(customized.text, /^dots sync: no local changes/);
  assert.doesNotMatch(customized.text, /previous copy of|COPY KEPT|APPLY FAILED/);
  assert.equal(git(f.origin, 'show', 'dev:kept.txt'), 'generated by pre-hook');
  assert.equal(fs.readFileSync(observation, 'utf8'), 'generated by pre-hook\ngenerated by pre-hook\ncustomized on this host\n');
});

test('real mise copies converge with one sync per machine for each edit', requires('git', 'jq', 'mise'), t => {
  const f = copiedFixture(t), a = f.machineA, b = f.machineB;
  for (const content of ['first edit\n', 'second edit\n']) {
    f.write(a.home, '.copied', content);
    const shared = a.sync(), pulled = b.sync();
    assert.equal(shared.code, 0);
    assert.equal(pulled.code, 0);
    assert.match(shared.text, /^dots sync: shared 1 file .* pushed/);
    assert.match(pulled.text, /^dots sync: no local changes .* nothing to push/);
    for (const [root, machine] of [[f.a, a], [f.b, b]]) {
      assert.equal(read(root, 'copy.txt'), content);
      assert.equal(read(machine.home, '.copied'), content);
      assert.equal(read(machine.home, '.rendered'), 'rendered\n');
      assert.equal(fs.readlinkSync(path.join(machine.home, '.linked')), path.join(root, 'link.txt'));
      f.clean(root);
    }
    assert.equal(f.dev(f.a), f.dev(f.b));
    assert.equal(f.dev(f.a), f.remote());
  }
});

test('real mise preserves a dirty copy across an incoming source edit and retry', requires('git', 'jq', 'mise'), t => {
  const f = copiedFixture(t), a = f.machineA, b = f.machineB;
  f.write(a.home, '.copied', 'my dirty copy\n');
  f.write(f.b, 'copy.txt', 'incoming edit\n');
  f.write(f.b, 'template.tera', '{{ "incoming template" }}\n');
  assert.match(b.sync().text, /^dots sync: shared 2 files/);
  const { code, text } = a.sync();
  assert.equal(code, 0);
  assert.match(text, /COPY KEPT: ~\/\.copied.*compare it with its source/);
  const found = text.match(/CONFLICT kept published: copy\.txt +\(yours: git show ([a-f0-9]+):copy\.txt\)/);
  assert(found, text);
  assert.equal(git(f.origin, 'show', `${found[1]}:copy.txt`), 'my dirty copy');
  assert.equal(read(f.a, 'copy.txt'), 'incoming edit\n');
  assert.equal(read(a.home, '.copied'), 'my dirty copy\n');
  assert.equal(read(a.home, '.rendered'), 'incoming template\n');
  assert.match(a.sync().text, /^dots sync: no local changes .* nothing to push/);
  assert.equal(read(a.home, '.copied'), 'my dirty copy\n');
  f.clean(f.a);
});

test('real mise keeps both locally diverged files and reports a capture write failure', requires('git', 'jq', 'mise'), t => {
  const f = copiedFixture(t), a = f.machineA;
  f.write(a.home, '.copied', 'local target\n');
  f.write(f.a, 'copy.txt', 'local source\n');
  const diverged = a.sync();
  assert.match(diverged.text, /COPY KEPT: ~\/\.copied: source .*copy\.txt also changed/);
  assert.equal(read(a.home, '.copied'), 'local target\n');
  assert.equal(git(f.origin, 'show', 'dev:copy.txt'), 'local source');
  f.write(f.a, 'copy.txt', 'initial\n');
  f.write(f.a, 'unrelated', 'share this\n');
  const rename = fs.renameSync;
  let result;
  fs.renameSync = (from, to) => {
    if (to === path.join(f.a, 'copy.txt')) throw new Error('injected copy write failure');
    return rename(from, to);
  };
  try { result = a.sync(); } finally { fs.renameSync = rename; }
  assert.equal(result.code, 0);
  assert.match(result.text, /CAPTURE FAILED: ~\/\.copied → .*copy\.txt: injected copy write failure/);
  assert.equal(read(a.home, '.copied'), 'local target\n');
  assert.equal(git(f.origin, 'show', 'dev:unrelated'), 'share this');
  assert.match(a.sync().text, /^dots sync: shared 1 file/);
  assert.equal(git(f.origin, 'show', 'dev:copy.txt'), 'local target');
});

test('real mise keeps a diverged copy after an incoming declaration replaces it', requires('git', 'jq', 'mise'), async t => {
  for (const [mode, declaration, deployed] of [
    ['absent', '{ mode = "absent" }', null],
    ['template', '{ source = "template.tera", mode = "template" }', 'updated template\n'],
    ['copy', '{ source = "new.txt", mode = "copy" }', 'initial\n'],
  ]) {
    await t.test(mode, t => {
      const f = copiedFixture(t), a = f.machineA, b = f.machineB;
      f.write(a.home, '.copied', 'keep my copy\n');
      f.write(f.a, 'copy.txt', 'keep my source\n');
      f.write(f.b, 'template.tera', '{{ "updated template" }}\n');
      f.write(f.b, 'new.txt', 'initial\n');
      const config = read(f.b, 'config.toml').replace('{ source = "copy.txt", mode = "copy" }', declaration);
      f.write(f.b, 'config.toml', config);
      assert.match(b.sync().text, /pushed/);
      assert.match(a.sync().text, /COPY KEPT: .*\.copied/);
      const shared = f.remote();
      const retry = a.sync();
      assert.match(retry.text, /COPY KEPT: .*\.copied.*move the deployed (?:copy|file) somewhere safe/);
      assert.equal(f.remote(), shared, 'a retry must not publish the dirty target through a new source');
      assert.equal(read(a.home, '.copied'), 'keep my copy\n');
      assert.equal(git(f.origin, 'show', 'dev:copy.txt'), 'keep my source');
      fs.renameSync(path.join(a.home, '.copied'), path.join(a.home, '.kept'));
      assert.doesNotMatch(a.sync().text, /COPY KEPT|APPLY FAILED/);
      if (deployed === null) assert(!fs.existsSync(path.join(a.home, '.copied')));
      else assert.equal(read(a.home, '.copied'), deployed);
      assert.equal(read(a.home, '.kept'), 'keep my copy\n');
    });
  }
});

test('real mise checks a former copy for edits before running a replacement declaration', requires('git', 'jq', 'mise'), async t => {
  for (const interrupted of [false, true]) {
    await t.test(interrupted ? 'edited after checkout failed' : 'edited before a local mode change', t => {
      const f = copiedFixture(t), a = f.machineA, b = f.machineB;
      const root = interrupted ? f.b : f.a;
      const absent = read(root, 'config.toml').replace('{ source = "copy.txt", mode = "copy" }', '{ mode = "absent" }');
      if (!interrupted) f.write(a.home, '.copied', 'keep my copy\n');
      f.write(root, 'config.toml', absent);
      if (interrupted) {
        assert.match(b.sync().text, /pushed/);
        failIndexInstall(f.a, a.sync);
        f.write(a.home, '.copied', 'keep my copy\n');
      }
      const published = f.remote();
      const { code, text } = a.sync();
      assert.equal(code, 0);
      assert.match(text, /COPY KEPT: .*\.copied.*move the deployed file somewhere safe/);
      assert.equal(read(a.home, '.copied'), 'keep my copy\n');
      if (interrupted) assert.equal(f.remote(), published);
      f.clean(f.a);
    });
  }
});

test('real mise preserves edited deployed files when an absent declaration arrives later', requires('git', 'jq', 'mise'), async t => {
  for (const remembered of [false, true]) {
    await t.test(remembered ? 'after the clean declaration was removed' : 'with only mise deployment state', t => {
      const f = copiedFixture(t), a = f.machineA;
      const config = read(f.a, 'config.toml');
      const copy = '"~/.copied" = { source = "copy.txt", mode = "copy" }\n';
      const absent = config.replace(copy, '"~/.copied" = { mode = "absent" }\n');
      if (remembered) {
        f.write(f.a, 'config.toml', config.replace(copy, ''));
        assert.match(a.sync().text, /^dots sync: shared 1 file/);
        assert.equal(read(a.home, '.copied'), 'initial\n');
      } else {
        // Simulate a copy deployed by mise before dots had seen it.
        fs.rmSync(path.join(a.home, '.copied'));
        a.deploy();
        fs.rmSync(path.join(f.a, '.git/dots-copied.json'));
      }
      f.write(a.home, '.copied', 'keep my copy\n');
      f.write(f.a, 'config.toml', absent);
      const first = a.sync(), retry = a.sync();
      for (const result of [first, retry]) {
        assert.equal(result.code, 0);
        assert.match(result.text, /COPY KEPT: .*\.copied.*move the deployed file somewhere safe/);
      }
      assert.equal(read(a.home, '.copied'), 'keep my copy\n');
      f.clean(f.a);
    });
  }
});

test('real mise can redeploy old bytes without reviving a stale copy baseline', requires('git', 'jq', 'mise'), t => {
  const f = copiedFixture(t), a = f.machineA;
  fs.rmSync(path.join(a.home, '.copied'));
  a.deploy();
  f.write(a.home, '.copied', 'captured once\n');
  assert.match(a.sync().text, /^dots sync: shared 1 file/);
  for (const content of ['another deployment\n', 'initial\n']) {
    f.write(f.a, 'copy.txt', content);
    a.deploy();
    assert.equal(read(a.home, '.copied'), content);
  }
  f.write(a.home, '.copied', 'captured once\n');
  const absent = read(f.a, 'config.toml').replace('{ source = "copy.txt", mode = "copy" }', '{ mode = "absent" }');
  f.write(f.a, 'config.toml', absent);
  assert.match(a.sync().text, /COPY KEPT: .*\.copied/);
  assert.equal(read(a.home, '.copied'), 'captured once\n');
});

test('a successful copy-to-template replacement restores ordinary real mise template updates', requires('git', 'jq', 'mise'), t => {
  const f = copiedFixture(t), a = f.machineA;
  const template = read(f.a, 'config.toml').replace('{ source = "copy.txt", mode = "copy" }', '{ source = "template.tera", mode = "template" }');
  f.write(f.a, 'config.toml', template);
  assert.doesNotMatch(a.sync().text, /COPY KEPT|APPLY FAILED/);
  assert.equal(read(a.home, '.copied'), 'rendered\n');
  f.write(a.home, '.copied', 'template output edited by hand\n');
  f.write(f.a, 'template.tera', '{{ "next template" }}\n');
  assert.doesNotMatch(a.sync().text, /COPY KEPT|APPLY FAILED/);
  assert.equal(read(a.home, '.copied'), 'next template\n');
});

// The same upgrade against real mise: its own validation rejects the stale
// declaration, and its apply leaves the rendered file in place.
test('upgrading an older host with real mise', requires('git', 'jq', 'mise'), async t => {
  for (const listed of [true, false]) {
    await t.test(listed ? 'one sync retires the listed declaration' : 'an unlisted declaration still fails validation', t => {
      const f = fixture(t);
      const home = path.join(f.dir, 'home'), system = path.join(f.dir, 'system');
      fs.mkdirSync(system);
      const miseBin = miseBinary(); // resolved before HOME changes
      Object.assign(process.env, { HOME: home, MISE_CONFIG_DIR: f.a, MISE_DATA_DIR: path.join(f.dir, 'data'), MISE_STATE_DIR: path.join(f.dir, 'state'),
        MISE_CACHE_DIR: path.join(f.dir, 'cache'), MISE_SYSTEM_CONFIG_DIR: system, MISE_TRUSTED_CONFIG_PATHS: f.a,
        MISE_CEILING_PATHS: fs.realpathSync(os.tmpdir()), MISE_AUTO_INSTALL: '0', MISE_NO_HOOKS: '1', MISE_AUTO_UPDATE: '0' });
      const sync = root => f.sync(root, { miseBin });
      f.write(f.b, 'templates/herdr.tera', 'theme = "{{ \'dark\' }}"\n'); sync(f.b); sync(f.a);
      f.write(f.a, 'config.local.toml', KEPT + HERDR); fs.chmodSync(path.join(f.a, 'config.local.toml'), 0o600);
      assert.match(sync(f.a).text, /^dots sync: no local changes/);
      const deployed = path.join(home, '.config/herdr/config.toml');
      assert.equal(fs.readFileSync(deployed, 'utf8'), 'theme = "dark"\n');
      fs.rmSync(path.join(f.b, 'templates'), { recursive: true });
      if (listed) f.write(f.b, 'dots/retired-declarations', 'dotfiles.herdr\ttemplates/herdr.tera\n');
      assert.match(sync(f.b).text, /pushed/);
      const { code, text } = sync(f.a);
      assert.equal(code, 0);
      if (listed) {
        assert.match(text, /^dots sync: no local changes · pulled dev \+1, main \+0 · nothing to push\n/);
        assert.match(text, RETIRED('config.local.toml'));
        assert.doesNotMatch(text, /APPLY FAILED/);
        assert.equal(read(f.a, 'config.local.toml'), KEPT);
      } else {
        assert.match(text, /\n {2}INVALID merged config: [^\n]*sources do not exist[^\n]*; nothing pushed or applied\n {4} *\[dotfiles\]\."~\/\.config\/herdr\/config\.toml": [^\n]*templates\/herdr\.tera(\n|$)/);
        assert.equal(read(f.a, 'config.local.toml'), KEPT + HERDR);
        assert(fs.existsSync(path.join(f.a, 'templates/herdr.tera')));
      }
      assert.equal(fs.readFileSync(deployed, 'utf8'), 'theme = "dark"\n');
    });
  }
});
