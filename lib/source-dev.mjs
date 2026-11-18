// Unpushed work is exactly origin/dev..dev. Task branches never enter this path.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { validateIncoming, validateIncomingIgnores } from './source-repo.mjs';
import { RETIRED_LIST, localDeclarationFile, parseRetiredList, retireDeclarations } from './retired-declarations.mjs';
import { applyMetadata, dependencyLayers, fromHome, materializeDirectory, materializeHandoff, queueReloads, runMise, shellQuote } from './source-apply.mjs';

export const privatePath = name => /^(config\.local\.toml|miserc\.toml|config\.host-[^/]+\.toml|mise(?:\.[^/]+)?\.lock|locks|\.setup-state)(\/|$)/i.test(name);
const ZERO = '0'.repeat(40);
const SETUP_COMMON = fileURLToPath(new URL('./setup-common.sh', import.meta.url));
// The save guard dots up uses: every tracked file must be declared encrypted.
const GUARD = 'source "$0" && bin=$1 dir=$2 && run() { "$bin" -C "$dir" "$@"; } && setup_tracked_history_paths run >/dev/null';
const short = commit => commit.slice(0, 12);
// Credentials in a URL (scheme://user:secret@host) never reach the output.
const redact = text => String(text || '').replace(/\x1b\[[0-9:;?]*[A-Za-z]/g, '').replace(/([a-z][a-z0-9+.-]*:\/\/)[^\s/?#]+@/gi, '$1***@');
const firstLine = text => redact(text).split('\n').map(line => line.trim()).find(Boolean)?.slice(0, 160) || 'unknown error';
// What a failed command printed: its last lines, indented under the attention
// line they explain (deeper than an attention line, so never mistaken for one).
// mise's own footer (its version and a --verbose hint) explains nothing.
const outputLines = text => redact(text).split(/\r?\n|\r/)
  .map(line => line.trimEnd()).filter(line => line.trim() && !/^mise ERROR (Version: |Run with --verbose)/.test(line.trim()));
const printed = result => [result.stdout, result.stderr, result.error?.message].filter(Boolean).join('\n');
const exitText = result => result.error?.code === 'ETIMEDOUT' ? 'timed out' : `exit ${result.status ?? 'error'}`;
function syncReport() {
  const lines = [], issues = [], notices = [];
  const notice = (message, legacy = true) => {
    const text = redact(message);
    notices.push(text);
    if (legacy) lines.push(text);
  };
  const issue = (summary, reason, action, line, output = '') => {
    const diagnostics = outputLines(output).slice(-20);
    if (line) lines.push(redact(line), ...diagnostics.map(text => `  ${text}`));
    issues.push({ summary: redact(summary), detail: [redact(reason), ...diagnostics].filter(Boolean).join('\n'), action: redact(action) });
  };
  const copyKept = (name, reason, action) => issue(`Copy kept at ${name}`, reason, action, `COPY KEPT: ${name}: ${reason}; ${action}`);
  return { lines, issues, notices, issue, notice, copyKept };
}
const missing = error => error.code === 'ENOENT' || error.code === 'ENOTDIR';
const lstat = file => {
  try { return fs.lstatSync(file); }
  catch (error) { if (!missing(error)) throw error; return null; }
};
const inside = (directory, file) => file.startsWith(directory + path.sep);
const overlaps = (a, b) => a === b || inside(a, b) || inside(b, a);
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const readRegular = file => fs.readFileSync(file, { flag: fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW });
const replacementError = error => error.code === 'EXDEV'
  ? 'Git metadata and the file are on different filesystems; use a checkout with Git metadata on the same filesystem'
  : error.message;
const writeAtomic = (file, bytes) => {
  const temp = fs.mkdtempSync(path.join(path.dirname(file), '.dots-write-'));
  try {
    const next = path.join(temp, 'file');
    fs.writeFileSync(next, bytes, { mode: 0o600 });
    fs.renameSync(next, file);
  } finally { fs.rmSync(temp, { recursive: true, force: true }); }
};

// Move the actual file before checking it. An exclusive link never overwrites
// a newly created target, and the old inode stays reachable for open editors.
function replaceFile(staging, { target, expected, expectedMode, bytes, mode }, beforeMove = () => {}) {
  const directory = fs.mkdtempSync(path.join(staging, '.dots-copy-'));
  const incoming = path.join(directory, 'incoming'), previous = path.join(directory, 'previous');
  let displaced = false;
  const unchanged = () => {
    const moved = lstat(previous);
    return moved?.isFile() && moved.mode === expectedMode && digest(readRegular(previous)) === expected;
  };
  try {
    fs.writeFileSync(incoming, bytes, { mode });
    fs.chmodSync(incoming, mode);
    beforeMove(previous);
    if (expected !== null) {
      fs.renameSync(target, previous);
      displaced = true;
      if (!unchanged()) throw new Error('changed before replacement');
    }
    fs.linkSync(incoming, target);
  } catch (error) {
    if (displaced) {
      try { fs.linkSync(previous, target); displaced = false; }
      catch { /* the old entry remains at previous */ }
    }
    return { error, previous: displaced ? previous : null };
  } finally {
    if (displaced) fs.rmSync(incoming, { force: true });
    else fs.rmSync(directory, { recursive: true, force: true });
  }
  if (!displaced) return {};
  let changed = true;
  try { changed = !unchanged(); } catch { /* report the preserved path */ }
  return { previous, changed };
}

function installCopy(root, gitDir, copy, { notice, copyKept }) {
  const { name, target, source, hash } = copy;
  const parent = path.dirname(target);
  fs.mkdirSync(parent, { recursive: true });
  const location = fs.realpathSync(parent);
  const staging = location === root || inside(root, location) ? gitDir : location;
  const { error, previous, changed } = replaceFile(staging, copy);
  const recovery = `recover the previous file from ${previous}; reconcile it with ${source}`;
  if (error) {
    const reason = error.message === 'changed before replacement' ? 'copy changed before installation'
      : error.code === 'EEXIST' ? 'another file claimed the target during installation'
      : `copy installation failed (${replacementError(error)})`;
    const action = previous ? recovery : `compare it with ${source} and reconcile the two files, then rerun`;
    copyKept(name, reason, action);
    return null;
  }
  if (previous) {
    if (changed) copyKept(name, 'the displaced copy was edited during installation', recovery);
    else notice(`previous copy of ${name}: ${previous}`);
  }
  return { source, deployed: copy.deployed, digest: hash };
}

function regularTree(root) {
  const files = new Map();
  const visit = directory => {
    for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
      const file = path.join(directory, entry.name);
      if (entry.isDirectory()) visit(file);
      else if (entry.isFile()) {
        const stat = lstat(file);
        if (stat?.isFile()) files.set(path.relative(root, file), { hash: digest(readRegular(file)), mode: stat.mode });
      }
    }
  };
  if (lstat(root)?.isDirectory()) visit(root);
  return files;
}

function directoryLeaf(root, relative) {
  const file = path.resolve(root, relative);
  if (!inside(root, file) || !lstat(root)?.isDirectory()) return null;
  for (let parent = path.dirname(file); parent !== root; parent = path.dirname(parent)) {
    if (!lstat(parent)?.isDirectory()) return null;
  }
  const stat = lstat(file);
  return stat?.isFile() ? { hash: digest(readRegular(file)), mode: stat.mode } : null;
}

function installDirectory(plan, copy, entry, report, written) {
  const { root, gitDir, miseBin, scratch } = plan;
  const { name, target, source, expected, baseline } = copy;
  if (!entry || typeof entry.opts !== 'object') throw new Error('cannot verify directory copy options with mise');
  const materialized = fs.mkdtempSync(path.join(scratch, 'copy-tree-'));
  const files = new Map(Object.entries(baseline));
  try {
    const staged = path.join(materialized, 'tree');
    materializeDirectory(miseBin, scratch, staged, source, entry.opts);
    const makeDirectory = destination => {
      const existing = lstat(destination);
      if (existing?.isDirectory()) return;
      if (existing) throw new Error(`${destination} obstructs a copied directory`);
      fs.mkdirSync(destination, { recursive: destination === target });
    };
    const visit = (directory, destination) => {
      try { makeDirectory(destination); }
      catch (error) {
        report.copyKept(destination, error.message, 'move the obstruction aside and rerun sync');
        return;
      }
      for (const item of fs.readdirSync(directory, { withFileTypes: true })) {
        const stagedFile = path.join(directory, item.name), deployed = path.join(destination, item.name);
        if (item.isDirectory()) { visit(stagedFile, deployed); continue; }
        if (!item.isFile()) continue;
        const relative = path.relative(target, deployed), before = Object.hasOwn(expected, relative) ? expected[relative] : null;
        const bytes = readRegular(stagedFile), hash = digest(bytes), mode = lstat(stagedFile).mode & 0o7777;
        const observed = before ?? directoryLeaf(target, relative);
        if (observed?.hash === hash && (observed.mode & 0o7777) === mode) {
          files.set(relative, { hash, mode });
          continue;
        }
        const common = files.get(relative);
        // Git can verify the old bytes of an adopted copy, but does not
        // store its full mode. A bytes-only baseline cannot authorize chmod.
        const commonMode = common?.mode ?? mode;
        if (observed && (!before || before.hash !== common?.hash || (before.mode & 0o7777) !== commonMode)) {
          report.copyKept(deployed, 'deployed file differs from its last common version', `reconcile it with ${path.join(source, relative)} and rerun sync`);
          continue;
        }
        const record = installCopy(root, gitDir, { name: path.join(name, relative), target: deployed,
          source: path.join(source, relative), expected: before?.hash ?? null, expectedMode: before?.mode, bytes, mode, hash }, report);
        if (record) { files.set(relative, { hash, mode }); written.push(deployed); }
      }
    };
    visit(staged, target);
    return { source, directory: Object.fromEntries(files) };
  } finally { fs.rmSync(materialized, { recursive: true, force: true }); }
}

function installRegular(plan, copy, file, report, written) {
  const permissions = file.permissions ? Number.parseInt(file.permissions, 8) : null;
  // Read the source after hooks and earlier dependency layers. The bytes
  // and default permissions come from the same open file.
  const fd = fs.openSync(copy.source, fs.constants.O_RDONLY | fs.constants.O_NONBLOCK);
  let bytes, mode;
  try {
    const stat = fs.fstatSync(fd);
    if (!stat.isFile()) throw new Error('copy source is not a regular file');
    mode = permissions ?? (stat.mode & 0o777);
    bytes = fs.readFileSync(fd);
  } finally { fs.closeSync(fd); }
  const hash = digest(bytes);
  if (hash === copy.expected && (permissions === null || (copy.expectedMode & 0o7777) === permissions)) {
    return { source: copy.source, deployed: copy.deployed, digest: hash };
  }
  const record = installCopy(plan.root, plan.gitDir, { ...copy, bytes, mode, hash }, report);
  if (record) written.push(copy.target);
  return record;
}

function installHandoff(plan, file, declaration, global, hasEdit, env, report, written) {
  const { root, gitDir, scratch } = plan;
  const target = fromHome(file.target);
  if (file.mode === 'absent' && !lstat(target) && !hasEdit) return true;
  if (file.mode !== 'template' || !declaration || declaration.group || declaration.opts?.remove_empty || hasEdit || typeof file.source !== 'string'
    || !global.has(file.origin?.config) || path.dirname(file.origin.config) !== root) {
    report.copyKept(file.target, 'cannot safely stage this first takeover', 'use a different target or restore the copy declaration, then rerun sync');
    return false;
  }
  const stage = fs.realpathSync(fs.mkdtempSync(path.join(scratch, 'copy-handoff-')));
  try {
    const output = path.join(stage, 'output'), source = fromHome(file.source);
    const replacement = materializeHandoff(plan, output, file, env);
    const record = installCopy(root, gitDir, { ...replacement, name: file.target, target, source, expected: null }, report);
    if (record) written.push(target);
    return Boolean(record);
  } finally { fs.rmSync(stage, { recursive: true, force: true }); }
}

export function installCopyPlan(plan, env = process.env) {
  const { root, miseBin, scratch, copies, targets, blocked, handoffs, reloadFile } = plan;
  const report = syncReport();
  const { lines, issues, notices, issue, copyKept } = report;
  const records = {}, written = [], completedHandoffs = [];
  const query = args => runMise(miseBin, root, ['bootstrap', 'dotfiles', ...args], env);
  const current = () => {
    const status = query(['status', '--json']);
    if (status.error || status.status !== 0) throw new Error(firstLine(printed(status)));
    return JSON.parse(status.stdout);
  };
  let failure;
  let finishReloads;
  try {
    const status = current();
    const { entries, reload, global } = applyMetadata(miseBin, scratch, status.files, env);
    finishReloads = queueReloads(miseBin, scratch, root, env, reload, reloadFile);
    const checked = new Map(copies.map(copy => [copy.target, copy]));
    const native = new Set(targets);
    const failed = new Set(blocked);
    const files = status.files.filter(file => checked.has(fromHome(file.target)) || native.has(file.target));
    const active = new Set(files.map(file => fromHome(file.target)));
    for (const copy of copies.filter(copy => !active.has(copy.target))) {
      failed.add(copy.target);
      copyKept(copy.name, 'its declaration changed during pre-dotfiles hooks', 'rerun sync');
    }
    const applyNative = names => {
      if (!names.length) return;
      const result = runMise(miseBin, root, ['bootstrap', 'dotfiles', 'apply', '--yes', '--', ...names], { ...env, MISE_NO_HOOKS: '1' });
      if (result.error || result.status !== 0) failure = { status: result.status, stderr: printed(result) };
    };
    for (const layer of dependencyLayers(files, entries, failed)) {
      const ready = layer.filter(({ file, wait }) => {
        const blockedBy = wait.find(target => failed.has(target));
        const target = fromHome(file.target);
        if (blockedBy) {
          failed.add(target);
          const action = 'reconcile that path and rerun sync';
          issue(`Did not apply ${file.target}`, `dependency ${blockedBy} was kept`, action,
            `NOT APPLIED: ${file.target}: dependency ${blockedBy} was kept; ${action}`);
          return false;
        }
        const recreated = (handoffs[target] ?? []).map(({ directory, name }) => path.resolve(directory, name)).find(leaf => lstat(leaf));
        if (recreated) {
          failed.add(target);
          copyKept(file.target, `former directory copy ${recreated} reappeared`, 'move that file somewhere safe, then rerun sync');
          return false;
        }
        return true;
      });
      const nativeReady = ready.filter(({ file }) => !checked.has(fromHome(file.target)) && !handoffs[fromHome(file.target)] && file.mode !== 'copy');
      applyNative(nativeReady.map(({ file }) => file.target));
      if (failure) break;
      for (const { file, wait } of ready) {
        const target = fromHome(file.target), copy = checked.get(target);
        try {
          if (!copy) {
            if (handoffs[target]) {
              const hasEdit = status.edits.some(edit => fromHome(edit.path) === target);
              if (installHandoff(plan, file, entries.get(target), global, hasEdit, env, report, written)) completedHandoffs.push(target);
              else failed.add(target);
            }
            continue;
          }
          if (file.mode !== 'copy' || typeof file.source !== 'string' || fromHome(file.source) !== copy.source) {
            throw new Error('its declaration changed during pre-dotfiles hooks');
          }
          const declaration = entries.get(target);
          if (declaration?.opts?.create_if_missing === true && lstat(target)) continue;
          if (copy.directory && file.state === 'applied' && !wait.length) continue;
          // Mise reports an existing create_if_missing target as applied even
          // when its bytes differ. It must remain outside sync's ownership.
          if (!declaration && file.state === 'applied' && copy.state === 'applied') continue;
          const kept = issues.length;
          const record = copy.directory ? installDirectory(plan, copy, declaration, report, written)
            : installRegular(plan, copy, file, report, written);
          if (record) records[target] = record;
          if (!record || issues.length > kept) failed.add(target);
          else if (handoffs[target]) completedHandoffs.push(target);
        } catch (error) {
          failed.add(target);
          copyKept(file.target, error.message, 'fix the issue and rerun sync');
        }
      }
    }
    if (!failure) applyNative([...new Set(status.edits.map(edit => edit.path).filter(name => native.has(name) && !status.files.some(file => file.target === name)))]);
  } catch (error) { failure = { status: 1, stderr: error.message }; }
  const reloadAction = 'check [history.reload] and run the matching reload commands';
  const reloadLines = [];
  // queueReloads only reports failures through this argument.
  try { if (finishReloads) finishReloads(written, reloadLines); }
  catch (error) { reloadLines.push(`RELOAD FAILED: ${error.message}; ${reloadAction}`); }
  for (const line of reloadLines) issue('Reload failed', line, reloadAction, line);
  return { lines, issues, notices, records, failure, completedHandoffs };
}

// Mise's deployment records hold the SHA-256 of the last bytes it wrote.
// Read the versioned single-file records without depending on their filenames.
function deployedCopies() {
  const state = process.env.MISE_STATE_DIR || path.join(process.env.XDG_STATE_HOME || path.join(os.homedir(), '.local/state'), 'mise');
  const directory = path.join(state, 'dotfiles', 'targets');
  const records = new Map();
  if (!lstat(directory)) return records;
  for (const name of fs.readdirSync(directory)) {
    if (!name.endsWith('.toml')) continue;
    const fd = fs.openSync(path.join(directory, name), fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW);
    let record, stat;
    try {
      record = fs.readFileSync(fd, 'utf8');
      stat = fs.fstatSync(fd, { bigint: true });
    } finally { fs.closeSync(fd); }
    const target = record.match(/^target = ("(?:[^"\\]|\\.)*"|'[^']*')$/m)?.[1];
    const hash = record.match(/^content_digest = "([a-f0-9]{64})"$/m)?.[1];
    if (target && hash && /^version = 1$/m.test(record)) {
      const epoch = [stat.dev, stat.ino, stat.mtimeNs, stat.ctimeNs].join(':');
      records.set(target.startsWith("'") ? target.slice(1, -1) : JSON.parse(target), { hash, epoch });
    }
  }
  return records;
}

export function syncDev(root, { gitDir, miseBin, gitleaks = 'gitleaks', host = os.hostname().split('.')[0], pushTimeout = 60000 }) {
  const { lines, issues, notices, issue, notice, copyKept } = syncReport();
  const groups = [];
  let start = null, saved, publishedDev, scanBlocked = false;
  const outcome = (header, after, status = issues.length ? 'attention' : 'complete') => ({ header, status, scanBlocked, lines, issues, notices, before: start, after,
    groups: saved && publishedDev && isAncestor(saved.after, publishedDev) ? groups.filter(group => group !== saved) : groups });
  if (inside(root, gitDir) && gitDir !== path.join(root, '.git')) {
    const reason = `Git metadata at ${gitDir} is in the worktree`;
    const action = 'move it outside the worktree before rerunning sync';
    issue('Cannot sync this checkout', reason, action);
    return outcome(`dots sync: failed · ${reason}; ${action}`, null, 'failed');
  }
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'dots-sync-'));
  const env = { ...process.env, GIT_TERMINAL_PROMPT: '0', GCM_INTERACTIVE: 'never', GIT_LITERAL_PATHSPECS: '1',
    GIT_SSH_COMMAND: process.env.GIT_SSH_COMMAND || 'ssh -oBatchMode=yes -oConnectTimeout=10' };
  for (const key of ['GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE', 'GIT_OBJECT_DIRECTORY']) delete env[key];
  const raw = (bin, args, options = {}) => spawnSync(bin, args, { cwd: root, env, encoding: 'utf8', timeout: 60000, maxBuffer: 64 * 1024 * 1024, ...options });
  const tryGit = (args, options) => raw('git', args, options);
  const git = (args, options) => {
    const result = tryGit(args, options);
    if (result.error || result.status !== 0) throw new Error(`git ${args[0]} failed: ${firstLine(result.stderr) || result.error?.message}`);
    return options?.encoding === null ? result.stdout : result.stdout.trim();
  };
  const withIndex = file => ({ env: { ...env, GIT_INDEX_FILE: file } });
  const rev = name => { const r = tryGit(['rev-parse', '--verify', '-q', `${name}^{commit}`]); return r.status === 0 ? r.stdout.trim() : null; };
  const tree = commit => git(['rev-parse', `${commit}^{tree}`]);
  const isAncestor = (a, b) => tryGit(['merge-base', '--is-ancestor', a, b]).status === 0;
  const recordSnapshot = (base, snapshot) => {
    if (saved || base === snapshot) return;
    saved = { kind: 'saved', before: base, after: snapshot };
    groups.push(saved);
  };
  const count = range => Number(tryGit(['rev-list', '--count', ...range]).stdout?.trim() || 0);
  const commitTree = (treeId, parents, message) => git(['-c', 'commit.gpgsign=false', 'commit-tree', treeId, ...parents.flatMap(p => ['-p', p]), '-m', message]);
  const nul = text => text.split('\0').filter(Boolean);
  const gitNames = (args, options) => nul(git(args, { ...options, encoding: null }).toString('utf8'));
  const entries = (commit, directories = false) => new Map(nul(git(['ls-tree', directories ? '-rtz' : '-rz', '--full-tree', commit])).map(entry => {
    const tab = entry.indexOf('\t');
    return [entry.slice(tab + 1), entry.slice(0, tab).split(' ')];
  }));
  const mise = (args, options) => raw(miseBin, ['-C', root, 'bootstrap', 'dotfiles', ...args], options);
  let historyRefusal;
  const historyAllowed = () => {
    const guard = raw('bash', ['-c', GUARD, SETUP_COMMON, miseBin, root]);
    if (!guard.error && guard.status === 0) return true;
    const output = outputLines(guard.stderr || guard.error?.message);
    const reason = (output.pop() || '').trim().replace(/^setup: /, '') || 'unknown error';
    if (reason !== historyRefusal) {
      issue('Tracked history save refused', reason, 'Fix the tracked history configuration and rerun sync.',
        `SAVE REFUSED: ${reason}; nothing saved`, output.join('\n'));
      historyRefusal = reason;
    }
    return false;
  };

  // A successful capture can leave source and target equal without another
  // deployment. Remember that equality until mise deploys a newer version.
  const copyRecord = path.join(gitDir, 'dots-copied.json');
  let captures, deployment;
  const lastDeployment = target => (deployment ??= deployedCopies()).get(target);
  const commonHash = (record, deployed) => record?.deployed === (deployed?.epoch ?? null) ? record.digest ?? deployed?.hash : deployed?.hash;
  const capturePending = (record, deployed) => record?.captured && record.deployed === (deployed?.epoch ?? null);
  const copyNotices = new Map();
  const copyNotice = (target, reason, action) => {
    const reported = copyNotices.get(target) ?? new Set();
    const notice = `${reason}; ${action}`;
    if (reported.has(notice)) return;
    reported.add(notice);
    copyNotices.set(target, reported);
    copyKept(target, reason, action);
  };
  const captureFailed = (reason, action = 'fix the error and rerun sync', output = '') =>
    issue('Copy capture failed', reason, action, `CAPTURE FAILED: ${reason}; ${action}`, output);
  const inventory = () => {
    const result = mise(['status', '--json']);
    try {
      if (result.error || result.status !== 0) throw new Error(exitText(result));
      const status = JSON.parse(result.stdout);
      if (!Array.isArray(status.files) || !Array.isArray(status.edits)) throw new Error('unexpected inventory');
      return status;
    } catch (error) {
      captureFailed(`mise bootstrap dotfiles status (${error.message})`, undefined, printed(result));
      return null;
    }
  };
  const inspectCopy = ({ target, source, state }) => {
    const targetFile = fromHome(target), sourceFile = fromHome(source);
    const local = lstat(targetFile), entry = lstat(sourceFile);
    const sourceIsLink = entry?.isSymbolicLink();
    const origin = sourceIsLink ? fs.statSync(sourceFile, { throwIfNoEntry: false }) : entry;
    const deployed = lastDeployment(targetFile);
    const record = captures[targetFile];
    const sourceContents = () => sourceIsLink ? fs.readFileSync(sourceFile) : readRegular(sourceFile);
    const copy = { targetFile, sourceFile, deployed, record, local, origin };
    if (!local) return copy;
    const replaced = record?.source && record.source !== sourceFile;
    if (!local.isFile() || !origin?.isFile()) return { ...copy, replaced, unsafe: state !== 'applied' };
    const bytes = readRegular(targetFile), sourceBytes = sourceContents();
    const current = digest(bytes), sourceHash = digest(sourceBytes);
    // A process can stop between reserving the new digest and writing the
    // source. The old digest still applies when the source confirms that.
    const base = capturePending(record, deployed) && sourceHash === record.previous
      ? record.previous : commonHash(record, deployed);
    return { ...copy, bytes, current, sourceHash, base, replaced,
      unsafe: current !== sourceHash && current !== base };
  };
  const replaceCaptures = next => {
    if (JSON.stringify(next) !== JSON.stringify(captures)) writeAtomic(copyRecord, JSON.stringify(next));
    captures = next;
  };
  const recoverCaptures = () => {
    const next = { ...captures };
    for (const [target, { recovery, source, ...record }] of Object.entries(captures)) {
      if (!recovery) continue;
      const directory = path.dirname(recovery);
      if (path.dirname(directory) !== gitDir || !path.basename(directory).startsWith('.dots-copy-') || path.basename(recovery) !== 'previous') {
        throw new Error(`invalid capture recovery path ${recovery} for ${source}; check ${copyRecord} before rerunning sync`);
      }
      if (!lstat(source) && lstat(recovery)) {
        try { fs.linkSync(recovery, source); }
        catch (error) {
          throw new Error(`${source} is missing after capture (${error.message}); restore it from ${recovery} and rerun sync`);
        }
      }
      if (lstat(recovery)) {
        notice(`previous source for ${target}: ${recovery}`);
        fs.rmSync(path.join(directory, 'incoming'), { force: true });
      } else fs.rmSync(directory, { recursive: true, force: true });
      next[target] = { source, ...record };
    }
    replaceCaptures(next);
  };
  const directoryBaseline = (base, target, source, entry) => {
    if (!entry || typeof entry.opts !== 'object') throw new Error('cannot verify directory copy options with mise');
    const verified = new Map();
    const temp = fs.mkdtempSync(path.join(scratch, 'copy-baseline-'));
    const remember = (staged, original, options, fromArchive = false) => {
      materializeDirectory(miseBin, scratch, staged, original, options);
      const files = regularTree(staged);
      for (const [name, { hash, mode }] of files) {
        const before = directoryLeaf(target, name);
        if (before?.hash !== hash || verified.has(name)) continue;
        if (fromArchive) verified.set(name, { hash });
        else if (before.mode === mode) verified.set(name, { hash, mode: mode & 0o7777 });
      }
      return files.size;
    };
    try {
      // Use the native target names for both versions, including dot_prefix
      // and exclusions. A pre-existing copy may still contain the Git base.
      const leaves = remember(path.join(temp, 'current'), source, entry.opts);
      const version = `${base}:${path.relative(root, source)}`;
      if (verified.size < leaves && inside(root, source) && tryGit(['cat-file', '-t', version]).stdout?.trim() === 'tree') {
        const prior = path.join(temp, 'prior-source');
        fs.mkdirSync(prior);
        const archive = git(['archive', version], { encoding: null });
        const unpack = raw('tar', ['-xf', '-', '-C', prior], { input: archive });
        if (unpack.error || unpack.status !== 0) throw new Error(firstLine(printed(unpack)));
        // The archive already restricts the old source to tracked files.
        remember(path.join(temp, 'previous'), prior, { ...entry.opts, manifest: undefined }, true);
      }
      return { source, directory: Object.fromEntries(verified) };
    } finally { fs.rmSync(temp, { recursive: true, force: true }); }
  };
  const captureCopies = base => {
    try {
      captures = lstat(copyRecord) ? JSON.parse(fs.readFileSync(copyRecord, 'utf8')) : {};
    } catch (error) {
      captureFailed(`cannot read copy records (${error.message})`);
      return null;
    }
    recoverCaptures();
    const status = inventory();
    if (!status) return null;
    let next = { ...captures };
    const sources = entries(base);
    const files = status.files.filter(file => file.mode === 'copy' && typeof file.source === 'string');
    let directoryOptions;
    for (const file of files) {
      const target = fromHome(file.target), source = fromHome(file.source);
      next[target] ??= { source };
      try {
        if (lstat(target)?.isDirectory() && fs.statSync(source, { throwIfNoEntry: false })?.isDirectory()) {
          if (next[target].source === source && !next[target].directory) {
            directoryOptions ??= applyMetadata(miseBin, scratch, files, env, false).entries;
            next[target] = directoryBaseline(base, target, source, directoryOptions.get(target));
          }
          continue;
        }
        if (file.state === 'applied') continue;
        const copy = inspectCopy(file);
        if (!copy.unsafe) continue;
        const relative = path.relative(root, copy.sourceFile);
        const eligible = copy.base && copy.bytes && copy.sourceHash === copy.base && !copy.replaced &&
          typeof file.origin?.config === 'string' && inside(root, fromHome(file.origin.config)) &&
          inside(root, copy.sourceFile) && fs.realpathSync(copy.sourceFile) === copy.sourceFile &&
          !privatePath(relative) && /^100[67]/.test(sources.get(relative)?.[0]) &&
          !inside(root, fs.realpathSync(copy.targetFile)) &&
          files.filter(other => fromHome(other.source) === copy.sourceFile).length === 1;
        if (!eligible) {
          const reason = copy.base && copy.sourceHash && copy.sourceHash !== copy.base ? `source ${file.source} also changed` : 'copy has no eligible source and common version';
          if (!copy.replaced) copyNotice(file.target, reason, 'compare it with its source and reconcile the two files, then rerun');
          continue;
        }
        const captured = { source: copy.sourceFile, digest: copy.current, deployed: copy.deployed?.epoch ?? null, captured: true };
        // Record the recovery path before moving the tracked source, so a
        // stopped capture cannot turn an absent path into a saved deletion.
        const result = replaceFile(gitDir, { target: copy.sourceFile, expected: copy.base,
          expectedMode: copy.origin.mode, bytes: copy.bytes, mode: copy.origin.mode & 0o777 }, recovery => {
          if (!readRegular(copy.targetFile).equals(copy.bytes)) throw new Error('target changed during capture');
          replaceCaptures({ ...next, [copy.targetFile]: { ...captured, previous: copy.base, recovery } });
        });
        const action = `restore ${copy.sourceFile} from ${result.previous} and rerun sync`;
        if (result.error) {
          if (result.previous && !lstat(copy.sourceFile)) return { incomplete: action };
          replaceCaptures(next);
          const reason = result.error.message === 'changed before replacement' ? 'source changed before capture' : replacementError(result.error);
          throw new Error(result.previous ? `${reason}; recover the previous source from ${result.previous}` : reason);
        }
        if (result.changed) {
          captureFailed(`${file.target} → ${file.source}: the displaced source was edited during capture`,
            `recover the previous source from ${result.previous} and reconcile both versions`);
        } else notice(`previous source for ${file.target}: ${result.previous}`);
        next = { ...next, [copy.targetFile]: captured };
      } catch (error) {
        captureFailed(`${file.target} → ${file.source}: ${error.message}`);
      }
    }
    try { replaceCaptures(next); }
    catch (error) {
      captureFailed(`cannot record captured copies (${error.message})`);
      return null;
    }
    return status;
  };

  // Let mise run the user hooks around the ordered apply. The anchor does
  // not change the live filesystem. Each inner native apply has hooks off.
  const applyWithCopies = (plan, applyEnv, hooksDisabled) => {
    const profile = `host-${path.basename(scratch)}`;
    const override = path.join(root, `config.${profile}.override.toml`);
    const reloadFile = path.join(root, `config.${profile}.local.toml`);
    const scoped = { env: { ...applyEnv, MISE_ENV: [applyEnv.MISE_ENV, profile].filter(Boolean).join(',') } };
    plan = { ...plan, override, reloadFile };
    const input = path.join(scratch, 'copy-plan.json'), output = path.join(scratch, 'copy-result.json');
    const anchor = path.join(scratch, 'copy-hook-anchor');
    const driver = path.join(scratch, 'install-copies.mjs');
    try {
      if (hooksDisabled) {
        const installed = installCopyPlan(plan, scoped.env);
        return { apply: installed.failure ?? { status: 0 }, installed };
      }
      fs.writeFileSync(input, JSON.stringify(plan), { mode: 0o600 });
      fs.writeFileSync(driver, `import fs from 'node:fs';
import { installCopyPlan } from ${JSON.stringify(import.meta.url)};
const [input, output] = process.argv.slice(2);
const result = installCopyPlan(JSON.parse(fs.readFileSync(input, 'utf8')));
fs.writeFileSync(output, JSON.stringify(result), { mode: 0o600 });
if (result.failure) { console.error(result.failure.stderr); process.exitCode = 1; }
`, { mode: 0o600 });
      const hook = [process.execPath, driver, input, output].map(shellQuote).join(' ');
      const config = `[bootstrap.hooks.pre-dotfiles]
description = "Install checked dotfile copies"
run = ${JSON.stringify(hook)}
[dotfiles]
${JSON.stringify(anchor)} = { mode = "absent" }
`;
      fs.writeFileSync(override, config, { flag: 'wx', mode: 0o600 });
      const loaded = raw(miseBin, ['-C', root, 'config', 'ls', '--json'], scoped);
      if (loaded.error || loaded.status !== 0) return { apply: loaded };
      if (!JSON.parse(loaded.stdout).some(file => file.path === override)) {
        return { apply: { status: 1, stderr: 'mise did not load the guarded copy override' } };
      }
      const apply = mise(['apply', '--yes', '--', anchor], scoped);
      const installed = lstat(output) ? JSON.parse(fs.readFileSync(output, 'utf8')) : null;
      return { apply, installed };
    } finally {
      fs.rmSync(reloadFile, { force: true });
      fs.rmSync(override, { force: true });
    }
  };

  const applyDotfiles = (before, refresh, allowHistory) => {
    const status = refresh ? inventory() : before;
    if (!status) return;
    const blocked = new Set();
    const next = { ...captures };
    const declared = new Set();
    const guarded = new Set(), directories = new Set(), installs = [];
    const replacements = new Set(status.files.filter(file => file.mode !== 'copy').map(file => fromHome(file.target)));
    const replaced = [];
    for (const file of status.files.filter(file => file.mode === 'copy' && typeof file.source === 'string')) {
      declared.add(fromHome(file.target));
      try {
        const copy = inspectCopy(file);
        if (copy.origin?.isDirectory() && (!copy.local || copy.local.isDirectory())) {
          guarded.add(copy.targetFile);
          directories.add(copy.targetFile);
          const baseline = copy.record?.source === copy.sourceFile ? copy.record.directory ?? {} : {};
          installs.push({ name: file.target, target: copy.targetFile, source: copy.sourceFile, directory: true, baseline,
            expected: Object.fromEntries(Object.keys(baseline).map(name => [name, directoryLeaf(copy.targetFile, name)])) });
          continue;
        }
        if (copy.origin?.isFile() || (!copy.origin && !copy.local)) guarded.add(copy.targetFile);
        if (file.state === 'applied' && copy.current && copy.current !== copy.sourceHash) continue;
        if (copy.unsafe || (capturePending(copy.record, copy.deployed) && copy.local && copy.current !== copy.sourceHash)) {
          blocked.add(copy.targetFile);
          if (copy.replaced) {
            copyNotice(file.target, `copy source changed from ${copy.record.source} to ${file.source}`, 'move the deployed copy somewhere safe, then rerun sync');
          } else if (!copyNotices.has(file.target)) {
            copyNotice(file.target, `deployed copy differs from ${file.source}`, 'compare it with its source and reconcile the two files, then rerun');
          }
        } else if (guarded.has(copy.targetFile) && (!copy.local || copy.local.isFile())) {
          installs.push({ name: file.target, target: copy.targetFile, source: copy.sourceFile,
            expected: copy.current ?? null, expectedMode: copy.local?.mode, state: file.state,
            deployed: copy.deployed?.epoch ?? null });
        } else if (copy.current && copy.current === copy.sourceHash) {
          next[copy.targetFile] = { source: copy.sourceFile, deployed: copy.deployed?.epoch ?? null, digest: copy.current };
        } else if (!copy.record || copy.replaced || !copy.local) {
          next[copy.targetFile] = { source: copy.sourceFile };
        }
      } catch (error) {
        const target = fromHome(file.target);
        blocked.add(target);
        copyNotice(file.target, error.message, 'fix the error and rerun sync');
      }
    }
    // Mise's digest can protect an edited deployed file from an explicit
    // removal even before dots has seen the original copy declaration.
    for (const file of status.files.filter(file => file.mode === 'absent')) {
      const target = fromHome(file.target);
      if (next[target] || !lstat(target)?.isFile()) continue;
      const deployed = lastDeployment(target);
      if (deployed) next[target] = { deployed: deployed.epoch, digest: deployed.hash };
    }
    for (const [target, record] of Object.entries(next)) {
      if (declared.has(target)) continue;
      try {
        const stat = lstat(target);
        if (!stat) {
          delete next[target];
          continue;
        }
        const deployed = lastDeployment(target);
        if (stat.isFile() && !capturePending(record, deployed) && digest(readRegular(target)) === commonHash(record, deployed)) {
          if (replacements.has(target)) replaced.push(target);
          continue;
        }
        blocked.add(target);
        const reason = record.source ? `declaration no longer uses copy source ${record.source}` : 'mise last deployed a different version';
        copyNotice(target, reason, 'move the deployed file somewhere safe, then rerun sync');
      } catch (error) {
        blocked.add(target);
        copyNotice(target, error.message, 'fix the error and rerun sync');
      }
    }
    try { replaceCaptures(next); }
    catch (error) {
      captureFailed(`cannot record protected copies (${error.message})`);
      return;
    }
    const copies = installs.filter(copy => ![...blocked].some(target => overlaps(target, copy.target)));
    const handoffs = new Map();
    for (const file of status.files) {
      if (file.mode === 'track') continue;
      const target = fromHome(file.target);
      for (const directory of directories) {
        if (!inside(directory, target)) continue;
        // Native writes cannot recheck a former copy at replacement time.
        // Keep its pathname out of native apply while it still exists.
        for (const name of Object.keys(next[directory]?.directory ?? {})) {
          const leaf = path.resolve(directory, name);
          if (!inside(directory, leaf) || !overlaps(leaf, target)) continue;
          if (lstat(leaf)) {
            blocked.add(target);
            copyNotice(file.target, `would replace former directory copy ${leaf}`, 'move that file somewhere safe, then rerun sync');
          } else handoffs.set(target, [...(handoffs.get(target) ?? []), { directory, name }]);
        }
      }
    }
    const excluded = [...blocked, ...guarded];
    const safe = name => !excluded.some(file => overlaps(file, fromHome(name)));
    // Native apply validates composed footprints before selecting targets.
    // A child declaration may own a path the directory copy excludes.
    const safeFile = name => {
      const target = fromHome(name);
      return !excluded.some(file => overlaps(file, target) && (blocked.has(file) || !directories.has(file) || !inside(file, target)));
    };
    const targets = excluded.length ? [...new Set([...status.files.filter(file => file.mode !== 'track' && safeFile(file.target)).map(file => file.target),
      ...status.edits.map(edit => edit.path).filter(safe)])] : [];
    const plan = { root, gitDir, miseBin, scratch, targets, blocked: [...blocked], handoffs: Object.fromEntries(handoffs),
      copies: copies.filter(copy => !blocked.has(copy.target)) };
    const hooksDisabled = /^(?:1|true|yes|on)$/i.test(env.MISE_NO_HOOKS || '');
    const orchestrate = plan.copies.length > 0 || handoffs.size > 0 || (blocked.size > 0 && targets.length > 0)
      || (!hooksDisabled && !targets.length && [...guarded].some(target => !blocked.has(target)));
    const applyEnv = allowHistory ? env : { ...env, MISE_HISTORY_ENABLED: '0' };
    const finished = { ...next };
    let apply, installed;
    try {
      if (orchestrate) ({ apply, installed } = applyWithCopies(plan, applyEnv, hooksDisabled));
      else if (!excluded.length || targets.length) apply = mise(['apply', '--yes', ...(targets.length ? ['--', ...targets] : [])], { env: applyEnv });
    } catch (error) { apply = { error }; }
    if (apply) {
      if (apply.error || apply.status !== 0) {
        const retry = excluded.length || !allowHistory ? 'sync' : 'apply';
        const reason = `mise bootstrap dotfiles apply (${exitText(apply)})`;
        const action = `fix the error below, then run dots ${retry}`;
        issue('Dotfile apply failed', reason, action, `APPLY FAILED: ${reason}; ${action}`, printed(apply));
      } else if (replaced.length) {
        for (const target of replaced.filter(safeFile)) delete finished[target];
      }
    }
    if (installed) {
      lines.push(...installed.lines);
      issues.push(...installed.issues);
      notices.push(...installed.notices);
      Object.assign(finished, installed.records);
      for (const target of installed.completedHandoffs) {
        for (const { directory, name } of handoffs.get(target) ?? []) {
          const leaves = { ...finished[directory].directory };
          delete leaves[name];
          finished[directory] = { ...finished[directory], directory: leaves };
        }
      }
    }
    try { replaceCaptures(finished); }
    catch (error) { captureFailed(`cannot record installed copies (${error.message})`); }
  };

  // gitleaks over a commit range, reported by file and line, never by content.
  const scan = range => {
    const report = path.join(scratch, `gitleaks-${Date.now()}.json`);
    const args = ['git', '--no-banner', '--redact', '--exit-code', '1', '--report-format', 'json', '--report-path', report,
      `--log-opts=--diff-merges=first-parent ${range.join(' ')}`];
    if (fs.existsSync(path.join(root, '.gitleaks.toml'))) args.push('--config', path.join(root, '.gitleaks.toml'));
    if (fs.existsSync(path.join(root, '.gitleaksignore'))) args.push('--gitleaks-ignore-path', path.join(root, '.gitleaksignore'));
    const result = raw(gitleaks, [...args, root], { timeout: 300000 });
    if (result.error) return { error: result.error.code === 'ENOENT' ? 'gitleaks is not installed' : result.error.message };
    if (result.status === 0) return { findings: [] };
    if (result.status !== 1) return { error: `gitleaks exited ${result.status}${outputLines(result.stderr).length ? `: ${outputLines(result.stderr).at(-1).trim().slice(0, 160)}` : ''}` };
    try {
      const findings = JSON.parse(fs.readFileSync(report, 'utf8')).map(f => ({ file: f.File, line: f.StartLine, rule: f.RuleID, commit: f.Commit }));
      return findings.length ? { findings } : { error: 'gitleaks reported findings it did not list' };
    } catch { return { error: 'gitleaks report unreadable' }; }
  };

  // Merge without touching live files. On a conflicting hunk the published side
  // (theirs) wins; binary, type and delete conflicts take its file whole. Both
  // parents stay in history, so the losing version remains recoverable.
  const merge = (ours, theirs, label) => {
    if (!theirs || ours === theirs || isAncestor(theirs, ours)) return ours;
    if (isAncestor(ours, theirs)) return theirs;
    // A file/directory conflict moves the file aside to PATH~COMMIT; resolve it at PATH.
    const original = name => name.replace(new RegExp(`~(?:${ours}|${theirs})(?:_\\d+)?$`), '');
    const conflicts = nul(tryGit(['merge-tree', '--write-tree', '-z', '--name-only', ours, theirs]).stdout || '').slice(1).map(original);
    const resolved = tryGit(['merge-tree', '--write-tree', '-z', '--name-only', '-X', 'theirs', ours, theirs]);
    if (resolved.status > 1 || resolved.error) throw new Error(`cannot merge ${label}: ${firstLine(resolved.stderr)}`);
    const [mergedTree, ...remaining] = resolved.stdout.split('\0\0')[0].split('\0').filter(Boolean);
    let result = mergedTree;
    if (resolved.status === 1) {
      const index = path.join(scratch, 'merge-index');
      git(['read-tree', mergedTree], withIndex(index));
      // Everything at or under each conflicted path becomes the published version.
      const paths = [...new Set(remaining.map(original))];
      const drop = [...remaining, ...nul(git(['ls-tree', '-r', '-z', '--name-only', '--full-tree', mergedTree, '--', ...paths]))];
      const published = nul(git(['ls-tree', '-r', '-z', '--full-tree', theirs, '--', ...paths])).map(entry => {
        const [meta, name] = entry.split('\t');
        const [mode, , oid] = meta.split(' ');
        return `${mode} ${oid}\t${name}\0`;
      });
      const input = drop.map(name => `0 ${ZERO}\t${name}\0`).join('') + published.join('');
      git(['update-index', '-z', '--index-info'], { ...withIndex(index), input });
      result = git(['write-tree'], withIndex(index));
    }
    const has = (commit, name) => tryGit(['cat-file', '-e', `${commit}:${name}`]).status === 0;
    for (const name of new Set(conflicts)) {
      const yours = has(ours, name);
      if (!yours && !has(theirs, name)) continue; // merge artifact
      const version = yours ? `git show ${short(ours)}:${name}` : `deleted in ${short(ours)}`;
      issue(`Conflict in ${name}`, `Kept the published version. Yours: ${version}`,
        'Recover your version from Git if needed, reconcile it with the published version, and rerun sync.',
        `CONFLICT kept published: ${name}  (yours: ${version})`);
    }
    return commitTree(result, [ours, theirs], `Merge ${label} (${host})`);
  };

  // Retired local declarations. Invariant: once the files on disk are at a
  // commit that retires declarations, each one is gone from exactly the bytes
  // validated without it, or a NOT RETIRED line says it is still there. The
  // checkout runs only while every local config file is still the bytes the
  // validated plan was read from (NOT APPLIED otherwise), and the plan is
  // written only over those same bytes; a writer racing in between gets
  // NOT RETIRED, never a silent stale declaration.
  const localInputs = () => new Map(fs.readdirSync(root).filter(localDeclarationFile)
    .filter(name => fs.lstatSync(path.join(root, name)).isFile()).map(name => [name, fs.readFileSync(path.join(root, name))]));
  // COMMIT's plan, or null when it retires nothing: the local files it read
  // (inputs, name -> bytes) and, where something matched, name -> { text, removed }.
  const retiredFor = commit => {
    const list = tryGit(['cat-file', 'blob', `${commit}:${RETIRED_LIST}`]);
    const entries = list.status === 0 ? parseRetiredList(list.stdout) : [];
    if (!entries.length) return null;
    const exists = name => tryGit(['cat-file', '-e', `${commit}:${name}`]).status === 0;
    const inputs = localInputs(), plans = new Map();
    for (const [name, bytes] of inputs) {
      const text = bytes.toString('utf8');
      if (!Buffer.from(text, 'utf8').equals(bytes)) continue; // not UTF-8: leave it to validation
      const plan = retireDeclarations(text, entries, exists);
      if (plan.removed.length) plans.set(name, plan);
    }
    return { inputs, plans };
  };
  // Local files that changed since RETIREMENT read them.
  const changedSince = retirement => {
    if (!retirement) return [];
    const now = localInputs();
    return [...new Set([...retirement.inputs.keys(), ...now.keys()])].filter(name => !now.get(name)?.equals(retirement.inputs.get(name))).sort();
  };
  // Only once the files on disk are the retiring commit's: the old files still
  // have the source, so the declaration stays valid until then. Deployed files stay.
  const retire = retirement => {
    for (const [name, { text, removed }] of retirement?.plans || []) {
      const file = path.join(root, name), backups = path.join(gitDir, 'dots-retired');
      if (!localInputs().get(name)?.equals(retirement.inputs.get(name))) {
        for (const entry of removed) {
          const declaration = `local declaration [${entry.table}] in ${name}`;
          const action = 'rerun, or remove the table by hand';
          issue('Local declaration kept', `${declaration}, which changed during sync`, action,
            `NOT RETIRED: ${declaration}, which changed during sync; ${action}`);
        }
        continue;
      }
      const mode = fs.statSync(file).mode & 0o777;
      fs.mkdirSync(backups, { recursive: true, mode: 0o700 });
      const backup = path.join(backups, `${name}.${new Date().toISOString().replace(/[-:]|\.\d+/g, '')}`);
      fs.copyFileSync(file, backup); fs.chmodSync(backup, 0o600);
      const temp = path.join(backups, `${name}.new-${process.pid}`);
      try { fs.writeFileSync(temp, text, { mode }); fs.chmodSync(temp, mode); fs.renameSync(temp, file); }
      finally { fs.rmSync(temp, { force: true }); }
      for (const entry of removed) {
        notice(`RETIRED local declaration [${entry.table}] from ${name} (source ${entry.source} no longer exists); deployed files left in place, backup ${path.relative(root, backup)}`);
      }
    }
  };

  const realIndex = path.join(gitDir, 'index');
  const indexLock = `${realIndex}.lock`;
  const indexLockName = path.relative(root, indexLock);
  const checkoutRecord = path.join(gitDir, 'dots-checkout.json');
  const lockIndex = () => {
    try { fs.closeSync(fs.openSync(indexLock, 'wx', 0o644)); return true; }
    catch (error) { if (error.code === 'EEXIST') return false; throw error; }
  };
  const commitIndex = (file, prepared) => {
    fs.renameSync(file, indexLock);
    // Until this succeeds, the original index is still intact.
    fs.renameSync(indexLock, realIndex);
    prepared.installed = true;
  };
  const moveDev = (to, from) => tryGit(['update-ref', '-m', 'dots sync', 'refs/heads/dev', to, from]).status === 0;
  // Called only after lstat found a path at this spelling. Find its actual
  // directory entry on a case-insensitive volume without following symlinks.
  const diskName = (directory, name) => {
    const children = fs.readdirSync(path.join(root, directory));
    if (children.includes(name)) return name;
    const alias = children.find(child => child.toLowerCase() === name.toLowerCase());
    if (!alias) throw new Error(`cannot resolve on-disk spelling of ${path.join(directory, name)}; move the obstructing path aside and rerun sync`);
    return alias;
  };
  // read-tree can replace ignored paths. Check new leaves and their parents
  // without walking through symlinks. A tracked directory can become a file
  // only when it contains exclusively tracked material.
  const checkoutCollisions = (from, to) => {
    const old = entries(from, true);
    const incoming = entries(to, true);
    const extra = (name, stat) => {
      const mode = old.get(name)?.[0];
      if (!mode) return name;
      if ((mode === '040000') !== stat.isDirectory()) return name;
      if (!stat.isDirectory()) return null;
      for (const child of fs.readdirSync(path.join(root, name))) {
        const nested = `${name}/${child}`, local = lstat(path.join(root, nested));
        const blocked = local && extra(nested, local);
        if (blocked) return blocked;
      }
      return null;
    };
    const blocked = new Set();
    for (const [name, [mode]] of incoming) {
      if (mode === '040000') continue;
      if (old.has(name) && old.get(name)[0] !== '040000') continue;
      const parts = name.split('/');
      let disk = '';
      for (let n = 1; n <= parts.length; n++) {
        const prefix = parts.slice(0, n).join('/');
        const part = parts[n - 1];
        let actual = path.join(disk, part);
        const stat = lstat(path.join(root, actual));
        if (!stat) break;
        // A case-insensitive volume can report the outgoing path at the new
        // spelling. Use the real directory entry, never a case-folded guess
        // on a case-sensitive volume or an exact untracked destination.
        if (!old.has(actual)) actual = path.join(disk, diskName(disk, part));
        if (n === parts.length) {
          const collision = actual !== prefix && incoming.has(actual) ? actual : extra(actual, stat);
          if (collision) blocked.add(collision);
        } else if (!stat.isDirectory()) {
          if (!old.has(actual) || old.get(actual)[0] === '040000' || (actual !== prefix && incoming.has(actual))) blocked.add(actual);
          break;
        }
        disk = actual;
      }
    }
    return [...blocked];
  };

  const worktreeEntry = name => {
    let actual = '', stat;
    const parts = name.split('/');
    for (const [i, part] of parts.entries()) {
      stat = lstat(path.join(root, actual, part));
      if (!stat || (i < parts.length - 1 && !stat.isDirectory())) return null;
      actual = path.join(actual, diskName(actual, part));
    }
    // The other spelling may also appear in the checkout diff. Observe the
    // actual directory entry exactly once so it cannot become a deletion.
    return actual === name ? stat : null;
  };

  // On retry, reconstruct only the interrupted paths in a temporary index.
  // Each may still hold its old version, its incoming version, or no file.
  // Never turn other bytes into a local snapshot while checkout is pending.
  const interruptedTree = (from, to, index) => {
    const before = entries(from), after = entries(to);
    const names = gitNames(['diff', '--no-renames', '--name-only', '-z', from, to]);
    const honorMode = git(['config', '--default', 'true', '--get', 'core.filemode']) !== 'false';
    const writes = [];
    for (const name of names) {
      const file = path.join(root, name);
      const stat = worktreeEntry(name);
      if (!stat || stat.isDirectory()) continue;
      const mode = stat.isSymbolicLink() ? '120000' : stat.mode & 0o100 ? '100755' : '100644';
      const object = stat.isSymbolicLink()
        ? git(['hash-object', '--stdin'], { input: fs.readlinkSync(file, { encoding: 'buffer' }) })
        : stat.isFile() ? git(['hash-object', `--path=${name}`, '--', name]) : null;
      const match = [before.get(name), after.get(name)].find(entry => entry && entry[2] === object &&
        (entry[0] === mode || (!honorMode && entry[0].startsWith('100') && mode.startsWith('100'))));
      if (!match) throw new Error(`${name} differs from both checkout versions; move it aside, rerun sync, then reconcile it with the incoming file`);
      writes.push(`${match[0]} ${match[2]}\t${name}\0`);
    }
    git(['read-tree', from], withIndex(index));
    const input = names.map(name => `0 ${ZERO}\t${name}\0`).join('') + writes.join('');
    git(['update-index', '-z', '--index-info'], { ...withIndex(index), input });
    return git(['write-tree'], withIndex(index));
  };

  const advance = (plan, retirement, { retry = false, base } = {}) => {
    if (!lockIndex()) {
      const reason = `the Git index is locked (${indexLockName})`;
      const action = 'finish any active Git operation, or remove that lock if it was left by an interrupted operation, then rerun sync';
      issue('Checkout blocked', reason, action, retry
        ? `NOT APPLIED: ${reason}; finish any active Git operation, or remove that lock if it was left by the interrupted sync, then rerun sync`
        : `NOT APPLIED: ${reason}; files not updated, rerun`);
      return false;
    }
    const prepared = { installed: false };
    let directory, pending = retry;
    try {
      const { snapshot, merged, dev } = plan;
      const head = rev('HEAD');
      if (head !== dev && (!retry || head !== merged)) throw new Error('dev moved; restore its previous position or finish the checkout by hand, then rerun sync');
      const late = changedSince(retirement);
      if (late.length) {
        const reason = `${late.join(', ')} changed during sync`;
        issue('Checkout blocked', reason, 'Rerun sync.', `NOT APPLIED: ${reason}; files not updated, rerun`);
        return false;
      }
      directory = fs.mkdtempSync(path.join(gitDir, 'dots-index-'));
      const index = path.join(directory, 'index');
      if (retry && lstat(realIndex)) fs.copyFileSync(realIndex, index);
      if (!retry || tryGit(['write-tree'], withIndex(index)).stdout?.trim() !== tree(merged)) {
        const from = retry ? interruptedTree(snapshot, merged, index) : snapshot;
        const collisions = checkoutCollisions(from, merged);
        if (collisions.length) {
          const action = 'move it aside, rerun sync, then reconcile it';
          for (const name of collisions) {
            const reason = `${name} obstructs an incoming tracked path`;
            issue(`Blocked path ${name}`, reason, action, `BLOCKED PATH: ${reason}; ${action}`);
          }
          return false;
        }
        git(['read-tree', from], withIndex(index));
        tryGit(['update-index', '-q', '--refresh'], withIndex(index));
        const args = ['read-tree', '-m', '-u', from, merged];
        const check = tryGit([...args, '-n'], withIndex(index));
        if (check.status !== 0) {
          const incoming = gitNames(['diff', '--name-only', '-z', from, merged]);
          const dirty = new Set(gitNames(['diff-files', '--name-only', '-z'], withIndex(index)));
          const blocked = incoming.filter(name => dirty.has(name));
          for (const name of blocked) {
            issue(`Late edit to ${name}`, 'The file changed during sync.', 'Rerun sync.', `LATE EDIT: ${name} not updated; rerun`);
          }
          if (!blocked.length) throw new Error(`checkout preflight failed: ${firstLine(printed(check))}`);
          return false;
        }
        if (!retry) writeAtomic(checkoutRecord, JSON.stringify(plan));
        pending = true;
        git(args, withIndex(index));
        commitIndex(index, prepared);
      }
      if (head !== merged && !moveDev(merged, dev)) throw new Error('could not update dev; finish the other Git operation, then rerun sync');
      if (base) recordSnapshot(base, snapshot);
      if (snapshot !== merged) groups.push({ kind: retry ? 'recovered' : 'received', before: snapshot, after: merged });
      const stat = tryGit(['diff', '--shortstat', snapshot, merged]).stdout?.trim();
      if (stat) lines.push(`changed on disk: ${stat}`);
      retire(retirement);
      fs.rmSync(checkoutRecord, { force: true });
      return true;
    } catch (error) {
      const action = pending ? 'checkout pending, rerun sync to finish it' : 'fix the error and rerun sync';
      issue('Checkout failed', error.message, action, `NOT APPLIED: ${error.message}; ${action}`);
      return false;
    } finally {
      if (!prepared.installed) fs.rmSync(indexLock, { force: true });
      if (directory) fs.rmSync(directory, { recursive: true, force: true });
    }
  };

  try {
    // 2. The files' base is the commit the index was checked out from. It equals
    // HEAD unless something moved dev without updating the files.
    start = rev('HEAD');
    let dev = start;
    if (lstat(checkoutRecord)) {
      const plan = JSON.parse(fs.readFileSync(checkoutRecord, 'utf8'));
      const retirement = retiredFor(plan.merged);
      validateIncoming(root, gitDir, plan.merged, miseBin, new Map([...retirement?.plans || []].map(([name, plan]) => [name, plan.text])));
      if (!advance(plan, retirement, { retry: true })) return outcome('dots sync: failed · checkout pending', rev('HEAD'), 'failed');
      dev = rev('HEAD');
    }
    const allowHistory = historyAllowed();
    if (allowHistory) {
      const save = mise(['save']);
      if (save.error || save.status !== 0) {
        const reason = `mise bootstrap dotfiles save (${exitText(save)})`;
        issue('Tracked history save failed', reason, 'Fix the mise error and rerun sync.',
          `SAVE FAILED: ${reason}; tracked history not saved`, printed(save));
      }
    }
    const indexTree = tryGit(['write-tree']).stdout?.trim();
    let base = dev;
    if (indexTree && indexTree !== tree(dev)) {
      const candidates = [['log', '--format=%H %T', '-n', '2000', 'HEAD'], ['log', '--format=%H %T', '-n', '500', 'refs/remotes/origin/dev'],
        ['log', '-g', '--format=%H %T', '-n', '200', 'refs/heads/dev'], ['log', '-g', '--format=%H %T', '-n', '200', 'HEAD']];
      for (const args of candidates) {
        const match = (tryGit(args).stdout || '').split('\n').find(line => line.endsWith(` ${indexTree}`));
        if (match) { base = match.split(' ')[0]; break; }
      }
      // A hand-staged index equal to an older commit looks the same; files on disk are still all captured.
      if (base !== dev) notice(`dev moved outside sync (now ${short(dev)}), or the index was staged to match ${short(base)}; local files were merged onto dev, not reverted`);
    }
    const copyInventory = captureCopies(base);
    if (copyInventory?.incomplete) {
      issue('Copy capture pending', 'A tracked source is missing after an interrupted replacement.',
        copyInventory.incomplete, `CAPTURE FAILED: ${copyInventory.incomplete}`);
      return outcome('dots sync: failed · capture pending', dev, 'failed');
    }

    // 3-4. Snapshot every tracked or unignored file onto the base, leaving out
    // private files and anything gitleaks flags.
    const index = path.join(scratch, 'index');
    if (fs.existsSync(realIndex)) {
      // Keep the index's own mtime: Git's racy-entry check compares against it.
      const stat = fs.statSync(realIndex);
      fs.copyFileSync(realIndex, index);
      fs.utimesSync(index, stat.atime, stat.mtime);
    }
    if (base !== dev || !indexTree) git(['read-tree', base], withIndex(index));
    git(['add', '-A', '--', '.'], withIndex(index));
    const baseEntries = entries(base);
    const restore = names => {
      if (!names.length) return;
      git(['update-index', '-z', '--index-info'], { ...withIndex(index), input: names.map(name => {
        const entry = baseEntries.get(name);
        return entry ? `${entry[0]} ${entry[2]}\t${name}\0` : `0 ${ZERO}\t${name}\0`;
      }).join('') });
    };
    const changed = () => nul(git(['diff-index', '--cached', '-z', '--name-only', base], withIndex(index)));
    restore(changed().filter(privatePath));
    let pushable = true;
    let snapshot = base, shared = 0;
    const snapshotTree = () => git(['write-tree'], withIndex(index));
    if (snapshotTree() !== tree(base)) {
      const tentative = commitTree(snapshotTree(), [base], `Save local setup (${host})`);
      const result = scan([`${base}..${tentative}`]);
      scanBlocked = Boolean(result.error || result.findings.length);
      if (result.error) {
        restore(changed());
        pushable = false;
        issue('Local edits held back', result.error, 'Install or fix gitleaks and rerun sync.',
          `NOT SHARED: ${result.error}; local edits kept on disk, nothing pushed`);
      } else {
        const flagged = [...new Set(result.findings.map(f => f.file))];
        for (const f of result.findings) {
          issue(`Secret scan held back ${f.file}`, `gitleaks ${f.rule} at ${f.file}:${f.line}`,
            'Remove the secret or allowlist a false positive, then rerun sync.', `NOT SHARED (gitleaks ${f.rule}): ${f.file}:${f.line}`);
        }
        restore(flagged);
      }
      if (snapshotTree() !== tree(base)) {
        shared = changed().length;
        snapshot = tree(tentative) === snapshotTree() ? tentative : commitTree(snapshotTree(), [base], `Save local setup (${host})`);
      }
    }
    // Record the snapshot under the index lock: dev moves first, and the index
    // is installed only once dev points at the snapshot.
    const record = () => {
      if (!lockIndex()) return `the Git index is locked (${indexLockName})`;
      const prepared = { installed: false };
      let directory;
      try {
        directory = fs.mkdtempSync(path.join(gitDir, 'dots-index-'));
        const copy = path.join(directory, 'index.install');
        fs.copyFileSync(index, copy);
        fs.utimesSync(copy, fs.statSync(index).atime, fs.statSync(index).mtime);
        if (!moveDev(snapshot, dev)) return 'dev is locked or moved (refs/heads/dev)';
        try { commitIndex(copy, prepared); } catch (error) { moveDev(dev, snapshot); throw error; }
        return null;
      } finally {
        if (!prepared.installed) fs.rmSync(indexLock, { force: true });
        if (directory) fs.rmSync(directory, { recursive: true, force: true });
      }
    };
    if (base === dev && snapshot !== base) {
      const failure = record();
      if (!failure) {
        dev = snapshot;
        recordSnapshot(base, snapshot);
      } else {
        issue('Local edits kept on disk', failure, 'Wait for the active Git operation to finish and rerun sync.',
          `NOT SAVED: ${failure}; local edits kept on disk, rerun`);
        snapshot = base;
        shared = 0;
      }
    }
    // Commits another worktree put on dev are dev material and win over local files.
    let local = base === dev ? snapshot : merge(snapshot, dev, 'local dev');

    // 5. Fetch; an unreachable origin means work continues offline.
    const tracked = branch => rev(`refs/remotes/origin/${branch}`);
    let remoteDev = tracked('dev'), remoteMain = tracked('main');
    const fetch = () => {
      let branches = ['dev', 'main'];
      const fetchBranches = names => tryGit(['fetch', '--no-tags', '--quiet', 'origin',
        ...names.map(name => `+refs/heads/${name}:refs/remotes/origin/${name}`)]).status === 0;
      if (!fetchBranches(branches)) {
        const listing = tryGit(['ls-remote', '--heads', 'origin']);
        if (listing.status !== 0) return false;
        const heads = new Set(listing.stdout.split('\n').map(line => line.split('\t')[1]).filter(Boolean));
        branches = branches.filter(name => heads.has(`refs/heads/${name}`));
        if (branches.length && !fetchBranches(branches)) return false;
      }
      remoteDev = branches.includes('dev') ? tracked('dev') : null;
      remoteMain = branches.includes('main') ? tracked('main') : null;
      publishedDev = remoteDev;
      return true;
    };
    let online = fetch();
    // A flagged existing commit blocks the push; local work still merges and applies.
    const published = () => [remoteDev, remoteMain].filter(Boolean).map(commit => `^${commit}`);
    if (pushable && online && count([local, ...published()]) > 0) {
      // Every unpublished commit, not only the merged tip, must leave private files alone.
      let commit, leaked;
      for (const token of nul(git(['log', '--format=%x01%H', '--name-only', '-z', local, ...published()]))) {
        if (token.startsWith('\x01')) commit = token.slice(1);
        else if (privatePath(token.replace(/^\n/, ''))) leaked = [commit, token.replace(/^\n/, '')]; // oldest wins
      }
      const result = leaked ? { error: `commit ${short(leaked[0])} changes private setup file ${leaked[1]}` } : scan([local, ...published()]);
      scanBlocked ||= Boolean(result.error || result.findings.length);
      if (result.error) {
        pushable = false;
        const action = leaked ? 'remove it from local history' : 'Install or fix gitleaks and rerun sync.';
        issue('Push blocked by the safety check', result.error, action, `NOT PUSHED: ${result.error}${leaked ? `; ${action}` : ''}`);
      } else if (result.findings.length) {
        pushable = false;
        for (const f of result.findings) {
          const location = `commit ${short(f.commit || '')} ${f.file}:${f.line}`;
          issue(`Secret scan blocked the push for ${f.file}`, `gitleaks ${f.rule}: ${location}`,
            'Remove the finding from local history and rerun sync.', `NOT PUSHED (gitleaks ${f.rule}): ${location}; remove it from local history`);
        }
      }
    }

    // 6-8. Merge local with dev, then main; validate; push without force.
    // Every attempt starts again from local, so main still wins over a racing dev.
    let merged, retirement = null, pushed = false, valid = true, devNew = 0, mainNew = 0, rejected = false;
    const lineCount = lines.length, issueCount = issues.length, noticeCount = notices.length;
    for (let attempt = 0; attempt <= 3; attempt++) {
      lines.length = lineCount; // only the last attempt's conflicts are reported
      issues.length = issueCount;
      notices.length = noticeCount;
      const attemptedRemote = remoteDev;
      devNew = remoteDev ? count([remoteDev, `^${local}`]) : 0;
      merged = merge(local, remoteDev, 'shared dev');
      mainNew = remoteMain ? count([remoteMain, `^${merged}`]) : 0;
      merged = merge(merged, remoteMain, 'reviewed main');
      try {
        if (nul(git(['ls-tree', '-r', '-z', '--name-only', '--full-tree', merged])).some(privatePath)) throw new Error('it tracks private setup files');
        // A directory is probed as NAME/ so directory-only rules (/locks/) match it.
        const privateNames = fs.readdirSync(root, { withFileTypes: true }).map(e => e.name + (e.isDirectory() ? '/' : '')).filter(privatePath);
        validateIncomingIgnores(root, gitDir, merged, new Set(nul(git(['ls-tree', '-z', '--name-only', '--full-tree', merged]))), privateNames);
        // Retired local declarations are validated as already gone; the files
        // themselves change only once the merged commit is on disk.
        retirement = retiredFor(merged);
        validateIncoming(root, gitDir, merged, miseBin, new Map([...retirement?.plans || []].map(([name, plan]) => [name, plan.text])));
      } catch (error) {
        valid = false;
        const reason = firstLine(error.message);
        issue('Invalid merged configuration', reason, 'Fix the configuration and rerun sync.',
          `INVALID merged config: ${reason}; nothing pushed or applied`, outputLines(error.message).slice(1).join('\n'));
        break;
      }
      if (!online || !pushable || merged === remoteDev) {
        if (attempt && merged === remoteDev) notice(`origin/dev reached ${short(merged)} during the push retry`, false);
        break;
      }
      // The accepted update identifies the actual previous tip, which can
      // differ from the fetched tip even when the push fast-forwards.
      const push = tryGit(['-c', 'core.abbrev=no', 'push', '--porcelain', 'origin', `${merged}:refs/heads/dev`], { timeout: pushTimeout });
      const stdout = (push.stdout || '').split('\n');
      const statusLine = stdout.findLastIndex(line => line.split('\t')[1]?.endsWith(':refs/heads/dev'));
      const [flag, , range] = stdout[statusLine]?.split('\t') ?? [];
      if (push.status === 0) {
        pushed = flag !== '=';
        publishedDev = merged;
        if (flag === '=') {
          notice(`origin/dev already pointed to ${short(merged)} when Git went to push`, false);
        } else if (flag === '*') {
          const emptyTree = git(['mktree'], { input: '' });
          groups.push({ kind: 'shared', before: emptyTree, after: merged });
          if (attemptedRemote) notice('origin/dev disappeared between fetch and push', false);
        } else {
          const [before, after] = range?.split('..') ?? [];
          if (flag === ' ' && before?.length === merged.length && /^[0-9a-f]+$/.test(before) && after === merged) {
            groups.push({ kind: 'shared', before, after });
            if (before !== attemptedRemote) notice(`origin/dev moved to ${short(before)} between fetch and push`, false);
          } else {
            notice('Git reported a successful push to origin/dev, but its previous tip could not be verified', false);
          }
        }
        tryGit(['update-ref', 'refs/remotes/origin/dev', merged]);
        break;
      }
      // Only a push that lost a race (origin/dev moved meanwhile) is retried;
      // anything else (credentials, network, a hook, a hang) is reported as it happened.
      if (push.error?.code === 'ETIMEDOUT') {
        const reason = `git push timed out after ${Math.round(pushTimeout / 1000)}s`;
        const action = 'check the Git credential helper for origin, then rerun';
        issue('Push timed out', reason, action, `NOT PUSHED: ${reason}; ${action}`, push.stderr);
        break;
      }
      if (!(online = fetch()) || remoteDev === attemptedRemote) {
        // Keep stderr diagnostics, with the ref's porcelain status as a fallback.
        const reportStart = stdout[statusLine - 1]?.startsWith('To ') ? statusLine - 1 : statusLine;
        const hookOutput = (reportStart < 0 ? stdout : stdout.slice(0, reportStart)).join('\n');
        const output = outputLines([hookOutput, push.stderr, push.error?.message].filter(Boolean).join('\n'))
          .filter(line => !/^(?:error: failed to push some refs|hint:)/.test(line));
        const reason = output.pop()?.trim().slice(0, 160) || range || exitText(push);
        issue('Push failed', reason, 'Fix the Git error and rerun sync.', `NOT PUSHED: git push failed: ${reason}`, output.join('\n'));
        break;
      }
      rejected = attempt === 3;
    }
    if (rejected) issue('Shared dev kept moving', 'The push lost every retry to another update.',
      'The next sync will retry.', 'NOT PUSHED: origin/dev kept moving; retried next run');
    if (!online) {
      const queued = count(['refs/heads/dev', ...published()]);
      issue('Origin unreachable', `${queued} unpushed commit${queued === 1 ? '' : 's'}`, 'The next sync will retry.',
        `OFFLINE: ${queued} commits unpushed, retried next run`);
    }

    // 9. Apply only once checkout, index and ref are aligned.
    let applied = false;
    if (valid && !copyInventory) issue('Checkout blocked', 'Copy safety could not be checked.', 'Fix the capture error and rerun sync.',
      'NOT APPLIED: copy safety could not be checked; fix the capture error and rerun sync');
    else if (valid && (merged !== dev || merged !== snapshot)) {
      applied = advance({ snapshot, merged, dev }, retirement, { base });
      if (applied) dev = merged;
    } else if (valid) { applied = true; retire(retirement); }
    if (applied) {
      const refresh = merged !== start || retirement?.plans.size;
      applyDotfiles(copyInventory, refresh, allowHistory && (!refresh || historyAllowed()));
    }

    const files = `${shared} file${shared === 1 ? '' : 's'}`;
    const parts = [
      !shared ? 'no local changes'
        : pushed ? `shared ${files}`
        : tryGit(['merge-base', '--is-ancestor', snapshot, 'refs/heads/dev']).status === 0 ? `saved ${files} locally`
        : `kept ${files} on disk`,
      !online ? 'offline' : `${applied ? 'pulled' : 'fetched'} dev +${devNew}, main +${mainNew}${applied ? '' : ' (not applied)'}`,
      pushed ? `pushed ${merged.slice(0, 7)}` : (online && valid && pushable && merged === publishedDev ? 'nothing to push' : 'not pushed'),
    ];
    return outcome(`dots sync: ${parts.join(' · ')}`, rev('HEAD'));
  } catch (error) {
    issue('Sync failed', error.message, 'Fix the reported error and rerun sync.');
    return outcome(`dots sync: failed · ${redact(error.message)}`, rev('HEAD'), 'failed');
  } finally { fs.rmSync(scratch, { recursive: true, force: true }); }
}
