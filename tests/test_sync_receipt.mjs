import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { buildReceipt, formatReceipt, inlinePatch, receiptChangeRows, saveReceipt } from '../lib/sync-receipt.mjs';

const modulePath = fileURLToPath(new URL('../lib/sync-receipt.mjs', import.meta.url));
const show = ['--input-type=module', '-e',
  'const { showLast } = await import(process.argv[1]); process.exitCode = showLast(process.argv[2]);', modulePath];

function fixture(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'dots-receipt-test-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const env = { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1' };
  delete env.DOTS_UP_RUN_DIR;
  delete env.DOTS_UP_STREAM_NONCE;
  delete env.DOTS_UP_EVENT_FD;
  const root = path.join(dir, 'work'), bin = path.join(dir, 'bin');
  fs.mkdirSync(root);
  fs.mkdirSync(bin);
  const run = (args, cwd = root, input) => {
    const result = spawnSync('git', args, { cwd, env, input, encoding: 'utf8' });
    assert.equal(result.status, 0, result.stderr);
    return result.stdout.trim();
  };
  const git = (...args) => run(args);
  git('init', '-q', '-b', 'dev');
  git('config', 'user.name', 'Receipt fixture');
  git('config', 'user.email', 'fixture@example.invalid');
  const write = (name, contents) => {
    const file = path.join(root, name);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, contents);
  };
  const commit = (message = 'Fixture') => {
    git('add', '-A');
    git('commit', '-qm', message);
    return git('rev-parse', 'HEAD');
  };
  const scanner = path.join(bin, 'gitleaks');
  fs.writeFileSync(scanner, `#!${process.execPath}
const fs = require('node:fs');
const mode = process.argv[2], name = process.argv.at(-1);
if (!['stdin', 'dir'].includes(mode) || !process.argv.includes('--ignore-gitleaks-allow')) process.exit(2);
const body = fs.readFileSync(mode === 'stdin' ? 0 : name, 'utf8');
const found = body.includes('SCAN_ALERT') || (mode === 'dir' && name === 'path/rule.yml' && body.includes('PATH_RULE'));
console.log(found ? '[{}]' : '[]');
process.exitCode = found ? 1 : 0;
`, { mode: 0o700 });
  const showLast = (at = root) => spawnSync(process.execPath, [...show, at], { env, encoding: 'utf8' });
  const receipt = (before, after, outcome = {}) => buildReceipt(root, {
    source: 'sync', groups: [{ kind: 'received', before, after }], ...outcome,
  });
  return { dir, root, bin, env, git, run, write, commit, scanner, showLast, receipt };
}

test('metadata preserves ranges, rename pairs, odd paths, and binary line counts', t => {
  const f = fixture(t);
  const odd = 'a\tfile\nname', renamed = 'new name';
  f.write(odd, 'unchanged\n');
  f.write('binary', Buffer.from([0, 1]));
  f.write('deleted', 'remove\n');
  const before = f.commit();
  fs.renameSync(path.join(f.root, odd), path.join(f.root, renamed));
  fs.rmSync(path.join(f.root, 'deleted'));
  f.write('binary', Buffer.from([0, 2]));
  f.write('new', 'one\ntwo\n');
  // Insert the byte path through the index. macOS cannot create it on disk.
  f.git('add', '-A');
  const blob = f.run(['hash-object', '-w', '--stdin'], f.root, 'one\n');
  f.run(['update-index', '-z', '--index-info'], f.root,
    Buffer.concat([Buffer.from('100644 ' + blob + '\tinvalid-'), Buffer.from([0xff, 0])]));
  f.git('commit', '-qm', 'Odd paths');
  const after = f.git('rev-parse', 'HEAD');
  const receipt = f.receipt(before, after, {
    header: 'dots sync: legacy header',
    lines: ['COPY KEPT: local edit', '  older version safely held', 'changed on disk: duplicate counts'],
    issues: [{ summary: 'COPY KEPT', detail: 'Move the local file aside.', action: 'Rerun dots sync.' }],
  });
  assert.deepEqual(receipt.groups[0], { kind: 'received', before, after, files: [
    { name: 'binary', operation: 'M', added: null, removed: null },
    { name: 'deleted', operation: 'D', added: 0, removed: 1 },
    { name: '"invalid-\\377"', operation: 'A', added: 1, removed: 0 },
    { name: 'new', operation: 'A', added: 2, removed: 0 },
    { name: '"new name"', from: '"a\\tfile\\nname"', operation: 'R', added: 0, removed: 0 },
  ] });
  const text = formatReceipt(receipt);
  assert.match(text, /^COPY KEPT\n  Move the local file aside\.\n  Rerun dots sync\./);
  assert.match(text, /Updated in this checkout · 5 files/);
  assert.match(text, /dots sync --last/);
  assert.doesNotMatch(text, /legacy header|duplicate counts|\t/);
  assert.match(text, /removed deleted|renamed .* → "new name"/);
  const explicit = f.receipt(before, after, {
    lines: ['Legacy issue'], notices: ['Configuration declaration retired'],
  });
  assert.match(formatReceipt(explicit), /Configuration declaration retired/);
  assert.doesNotMatch(formatReceipt(explicit), /Legacy issue/);
});

test('format shows a clean sync in one line and limits each direction to five paths', t => {
  const f = fixture(t);
  assert.equal(formatReceipt(buildReceipt(f.root, { header: 'dots sync: no local changes · nothing to push' })), 'Sync is up to date.');
  f.write('base', '');
  const before = f.commit();
  for (let i = 0; i < 6; i++) f.write('change' + i, 'new\n');
  const after = f.commit();
  const groups = ['shared', 'saved', 'received', 'recovered'].map(kind => ({ kind, before, after }));
  const receipt = f.receipt(before, after, { groups });
  const text = formatReceipt(receipt);
  assert.match(text, /Shared on dev/);
  assert.match(text, /Saved locally/);
  assert.match(text, /Updated in this checkout/);
  assert.match(text, /Finished pending checkout/);
  assert.equal(text.match(/… 1 more/g).length, groups.length);
  assert.doesNotMatch(text, /change5/);
  assert.deepEqual(receiptChangeRows(receipt), text.split('\n').slice(0, -1));
});

test('latest finished receipt is atomic worktree-local metadata and reports pruned endpoints', t => {
  const f = fixture(t);
  f.write('file', 'original\n');
  const before = f.commit();
  f.write('file', 'PAYLOAD_MUST_NOT_PERSIST\n');
  const after = f.commit();
  const receipt = f.receipt(before, after);
  receipt.patch = 'PAYLOAD_MUST_NOT_PERSIST';
  receipt.groups[0].patch = receipt.patch;
  receipt.groups[0].files[0].patch = receipt.patch;
  assert.equal(saveReceipt(f.root, receipt), true);
  const receiptFile = path.join(f.root, '.git', 'dots-sync-receipt.json');
  assert.equal(fs.statSync(receiptFile).mode & 0o777, 0o600);
  assert.doesNotMatch(fs.readFileSync(receiptFile, 'utf8'), /PAYLOAD_MUST_NOT_PERSIST|patch/);
  assert.doesNotMatch(fs.readdirSync(path.dirname(receiptFile)).join('\n'), /\.dots-receipt-/);
  const first = f.showLast();
  assert.equal(first.status, 0, first.stderr);
  assert.match(first.stdout, new RegExp(before + ' → ' + after));
  assert.doesNotMatch(first.stdout, /PAYLOAD_MUST_NOT_PERSIST|diff --git/);

  const linked = path.join(f.dir, 'linked');
  f.git('worktree', 'add', '-q', '-b', 'other', linked);
  assert.equal(f.showLast(linked).status, 1);
  const stopped = buildReceipt(linked, { header: 'dots sync: stopped · Git operation in progress', source: 'up' });
  assert.equal(saveReceipt(linked, stopped), true);
  const nested = f.showLast(linked);
  assert.equal(nested.status, 0, nested.stderr);
  assert.match(nested.stdout, /^Sync stopped\n  Git operation in progress/);
  assert.match(nested.stdout, /during dots up · stopped/);

  // Remove the loose endpoint in the fixture, even though its ref still names it.
  fs.unlinkSync(path.join(f.root, '.git', 'objects', after.slice(0, 2), after.slice(2)));
  assert.match(f.showLast().stdout, /recorded Git objects may have been pruned/);
  assert.doesNotMatch(f.showLast().stdout, /Git operation in progress/);

  fs.unlinkSync(receiptFile);
  fs.symlinkSync(path.join(f.dir, 'outside.json'), receiptFile);
  assert.equal(f.showLast().status, 1);
});

test('inline shows only a complete bounded patch from immutable Git endpoints', t => {
  const f = fixture(t);
  f.write('.gitattributes', 'text diff=marker\n');
  f.write('text', 'first\noriginal\nlast\n');
  const before = f.commit();
  f.write('text', 'first\nupdated\nlast\n');
  const after = f.commit();
  const receipt = f.receipt(before, after);
  f.write('text', 'uncommitted\n');
  const replacement = f.commit();
  f.git('replace', before, replacement);
  const marker = path.join(f.dir, 'external-called'), external = path.join(f.dir, 'external');
  fs.writeFileSync(external, '#!/bin/sh\ntouch "' + marker + '"\n', { mode: 0o700 });
  f.git('config', 'diff.external', external);
  f.git('config', 'diff.marker.command', external);
  f.git('config', 'diff.marker.textconv', external);
  f.write('.gitattributes', 'text -diff\n');

  const options = { gitleaks: f.scanner, width: 80 };
  const patch = inlinePatch(f.root, receipt, options);
  assert.equal(patch, 'Updated in this checkout\n  edited text (+1 −1)\n     first\n    -original\n    +updated\n     last\n');
  assert.doesNotMatch(patch, /diff --git|index |@@|--- a\/|\+\+\+ b\//);
  assert.doesNotMatch(patch, /uncommitted/);
  assert.equal(fs.existsSync(marker), false);
  assert.equal(inlinePatch(f.root, receipt, { ...options, width: 12 }), '');
  assert.equal(inlinePatch(f.root, { ...receipt, source: 'up' }, options), '');
  assert.equal(inlinePatch(f.root, { ...receipt, status: 'attention' }, options), '');
  const two = { ...receipt, groups: [...receipt.groups, ...receipt.groups] };
  assert.equal(inlinePatch(f.root, two, options).trimEnd().split('\n').length, 12);
  assert.equal(inlinePatch(f.root, { ...receipt, groups: [...two.groups, ...receipt.groups] }, options), '');

  const terminalEdit = (content, width = 100) => {
    f.write('text', content);
    const end = f.commit();
    return inlinePatch(f.root, f.receipt(after, end), { ...options, width });
  };
  assert.equal(terminalEdit('long\n'.repeat(20)), '');
  assert.equal(terminalEdit('x'.repeat(5000) + '\n', 6000), '');
});

test('inline suppresses binary, unsafe paths and bytes, secrets on either side, and scan failure', t => {
  const f = fixture(t);
  f.write('removed', 'SCAN_ALERT\n');
  f.write('context', 'SCAN_ALERT\nold\n');
  f.write('binary', Buffer.from([0, 1]));
  f.write('config.local.toml', 'local\n');
  f.write('old.age', 'age ciphertext\n');
  f.write('ordinary', 'plain\n');
  f.write('path/rule.yml', 'PATH_RULE\n');
  const before = f.commit();
  const options = { gitleaks: f.scanner, width: 100 };
  const cases = [
    ['removed', null],
    ['path/rule.yml', 'ordinary\n'],
    ['context', 'SCAN_ALERT\nnew\n'],
    ['binary', Buffer.from([0, 2])],
    ['config.local.toml', 'private\n'],
    ['old.age', null],
    ['ordinary', 'age-encryption.org/v1\n'],
    ['ordinary', 'unsafe \x1b[31m\n'],
    ['ordinary', 'bidi \u202e direction\n'],
    ['ordinary', Buffer.from([0xff, 10])],
    ['ordinary', 'SCAN_ALERT\n'],
    ['odd\nname', 'safe\n'],
  ];
  for (const [file, contents] of cases) {
    f.git('reset', '--hard', before);
    if (contents === null) fs.rmSync(path.join(f.root, file));
    else f.write(file, contents);
    const after = f.commit();
    assert.equal(inlinePatch(f.root, f.receipt(before, after), options), '', file);
  }
  f.git('reset', '--hard', before);
  f.write('ordinary', 'visible\n');
  const after = f.commit();
  assert.equal(inlinePatch(f.root, f.receipt(before, after, {
    lines: ['NOT SHARED (gitleaks custom-rule): ordinary:1'],
  }), options), '');
  assert.equal(inlinePatch(f.root, f.receipt(before, after), { ...options, gitleaks: path.join(f.dir, 'missing') }), '');
  for (const contents of ['exit 2', 'echo "scanner did not run"']) {
    const failed = path.join(f.dir, 'scanner-failure');
    fs.writeFileSync(failed, '#!/bin/sh\n' + contents + '\n', { mode: 0o700 });
    assert.equal(inlinePatch(f.root, f.receipt(before, after), { ...options, gitleaks: failed }), '');
  }
  assert.match(inlinePatch(f.root, f.receipt(before, after), options), /\+visible/);
});

test('inline permits two files and suppresses a receipt with three', t => {
  const f = fixture(t);
  for (const name of ['first', 'second', 'third']) f.write(name, 'body\n');
  const before = f.commit();
  for (const name of ['first', 'second']) fs.chmodSync(path.join(f.root, name), 0o755);
  const afterTwo = f.commit();
  fs.chmodSync(path.join(f.root, 'third'), 0o755);
  const afterThree = f.commit();
  const options = { gitleaks: f.scanner, width: 100 };
  const patch = inlinePatch(f.root, f.receipt(before, afterTwo), options);
  assert.equal(patch.match(/^  edited /gm)?.length, 2);
  assert.equal(inlinePatch(f.root, f.receipt(before, afterThree), options), '');
});

test('interactive last screens large mixed groups and opens restricted less for search and quit', { skip:
  spawnSync('python3', ['--version']).status !== 0 || spawnSync('less', ['--version']).status !== 0,
}, t => {
  const f = fixture(t);
  f.write('text', 'original\n');
  for (let i = 0; i < 22; i++) f.write('binary-' + i, Buffer.from([0, 1]));
  const before = f.commit();
  f.write('text', 'new display body\n');
  for (let i = 0; i < 22; i++) f.write('binary-' + i, Buffer.from([0, 2]));
  const after = f.commit();
  saveReceipt(f.root, f.receipt(before, after, { source: 'up' }));
  const stamp = path.join(f.dir, 'pager-shell-escape');
  const driver = String.raw`
import errno, fcntl, os, pty, re, select, signal, struct, sys, termios, time

command = sys.argv[1:]
pid, tty = pty.fork()
if not pid:
    fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 9, 100, 0, 0))
    os.execv(command[0], command)

output = bytearray()
def plain():
    return re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", output)

def expect(needle):
    deadline = time.monotonic() + 6
    while needle not in plain() and time.monotonic() < deadline:
        if select.select([tty], [], [], 0.1)[0]:
            try:
                output.extend(os.read(tty, 4096))
            except OSError as error:
                if error.errno == errno.EIO:
                    break
                raise
    if needle not in plain():
        raise RuntimeError(repr(bytes(output)))

try:
    expect(b"Latest sync")
    os.write(tty, b"/Patch omitted for 22 files\n")
    expect(b"Patch omitted for 22 files.")
    output.clear()
    os.write(tty, b"/new display body\n")
    expect(b"+new display body")
    os.write(tty, b"!")
    expect(b"not available")
    output.clear()
    os.write(tty, b"\r")
    expect(b"(END)")
    os.write(tty, b"q")
    deadline = time.monotonic() + 4
    while time.monotonic() < deadline:
        finished, status = os.waitpid(pid, os.WNOHANG)
        if finished:
            pid = None
            assert os.waitstatus_to_exitcode(status) == 0, status
            break
        if select.select([tty], [], [], 0.05)[0]:
            try:
                output.extend(os.read(tty, 4096))
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
    else:
        raise RuntimeError("pager did not quit: " + repr(bytes(output)))
finally:
    if pid is not None:
        try:
            os.killpg(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
`;
  const result = spawnSync('python3', ['-c', driver, process.execPath, ...show, f.root], {
    env: { ...f.env, TERM: 'xterm', PATH: f.bin + path.delimiter + f.env.PATH, LESS: '-F',
      LESSSECURE_ALLOW: 'shell', LESSOPEN: '|touch ' + stamp },
    encoding: 'utf8', timeout: 20000,
  });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(fs.existsSync(stamp), false);
});
