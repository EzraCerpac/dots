// Unpushed work is exactly origin/dev..dev. Task branches never enter this path.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { validateIncoming, validateIncomingIgnores } from './source-repo.mjs';
import { RETIRED_LIST, localDeclarationFile, parseRetiredList, retireDeclarations } from './retired-declarations.mjs';

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
const detail = (text, limit = 20) => outputLines(text).slice(-limit).map(line => `  ${line}`);
const printed = result => [result.stdout, result.stderr, result.error?.message].filter(Boolean).join('\n');
const exitText = result => result.error?.code === 'ETIMEDOUT' ? 'timed out' : `exit ${result.status ?? 'error'}`;
const missing = error => error.code === 'ENOENT' || error.code === 'ENOTDIR';
const lstat = file => {
  try { return fs.lstatSync(file); }
  catch (error) { if (!missing(error)) throw error; return null; }
};
const inside = (directory, file) => file.startsWith(directory + path.sep);
const overlaps = (a, b) => a === b || inside(a, b) || inside(b, a);
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const fromHome = name => path.resolve(name.startsWith('~/') ? path.join(os.homedir(), name.slice(2)) : name);
const writeAtomic = (file, bytes, mode = 0o600) => {
  const temp = fs.mkdtempSync(path.join(path.dirname(file), '.dots-write-'));
  try {
    const next = path.join(temp, 'file');
    fs.writeFileSync(next, bytes, { mode });
    fs.chmodSync(next, mode);
    fs.renameSync(next, file);
  } finally { fs.rmSync(temp, { recursive: true, force: true }); }
};

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
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'dots-sync-'));
  const env = { ...process.env, GIT_TERMINAL_PROMPT: '0', GCM_INTERACTIVE: 'never', GIT_LITERAL_PATHSPECS: '1',
    GIT_SSH_COMMAND: process.env.GIT_SSH_COMMAND || 'ssh -oBatchMode=yes -oConnectTimeout=10' };
  for (const key of ['GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE', 'GIT_OBJECT_DIRECTORY']) delete env[key];
  const lines = [];
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
  const count = range => Number(tryGit(['rev-list', '--count', ...range]).stdout?.trim() || 0);
  const commitTree = (treeId, parents, message) => git(['-c', 'commit.gpgsign=false', 'commit-tree', treeId, ...parents.flatMap(p => ['-p', p]), '-m', message]);
  const nul = text => text.split('\0').filter(Boolean);
  const gitNames = (args, options) => nul(git(args, { ...options, encoding: null }).toString('utf8'));
  const entries = (commit, directories = false) => new Map(nul(git(['ls-tree', directories ? '-rtz' : '-rz', '--full-tree', commit])).map(entry => {
    const tab = entry.indexOf('\t');
    return [entry.slice(tab + 1), entry.slice(0, tab).split(' ')];
  }));
  const mise = (args, options) => raw(miseBin, ['-C', root, 'bootstrap', 'dotfiles', ...args], options);
  const historyAllowed = () => {
    const guard = raw('bash', ['-c', GUARD, SETUP_COMMON, miseBin, root]);
    if (!guard.error && guard.status === 0) return true;
    const output = outputLines(guard.stderr || guard.error?.message);
    const reason = (output.pop() || '').trim().replace(/^setup: /, '');
    const notice = `SAVE REFUSED: ${reason || 'unknown error'}; nothing saved`;
    if (!lines.includes(notice)) lines.push(notice, ...detail(output.join('\n')));
    return false;
  };

  // A successful capture can leave source and target equal without another
  // deployment. Remember that equality until mise deploys a newer version.
  const copyRecord = path.join(gitDir, 'dots-copied.json');
  let captures, deployment;
  const lastDeployment = target => (deployment ??= deployedCopies()).get(target);
  const commonHash = (record, deployed) => record?.deployed === (deployed?.epoch ?? null) ? record.digest ?? deployed?.hash : deployed?.hash;
  const readRegular = file => fs.readFileSync(file, { flag: fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW });
  const copyNotice = (target, reason, action) => {
    const notice = `COPY KEPT: ${target}: ${reason}; ${action}`;
    if (!lines.includes(notice)) lines.push(notice);
  };
  const inventory = () => {
    const result = mise(['status', '--json']);
    try {
      if (result.error || result.status !== 0) throw new Error(exitText(result));
      const status = JSON.parse(result.stdout);
      if (!Array.isArray(status.files) || !Array.isArray(status.edits)) throw new Error('unexpected inventory');
      return status;
    } catch (error) {
      lines.push(`CAPTURE FAILED: mise bootstrap dotfiles status (${error.message}); fix the error and rerun sync`, ...detail(printed(result)));
      return null;
    }
  };
  const inspectCopy = ({ target, source, state }) => {
    const targetFile = fromHome(target), sourceFile = fromHome(source);
    const local = lstat(targetFile), origin = lstat(sourceFile);
    const deployed = lastDeployment(targetFile);
    const record = captures[targetFile];
    const copy = { targetFile, sourceFile, deployed, record, local };
    if (!local) return copy;
    const replaced = record?.source && record.source !== sourceFile;
    if (!local.isFile() || !origin?.isFile()) return { ...copy, replaced, unsafe: state !== 'applied' };
    const bytes = readRegular(targetFile), sourceBytes = readRegular(sourceFile);
    const current = digest(bytes), sourceHash = digest(sourceBytes), base = commonHash(record, deployed);
    return { ...copy, origin, bytes, sourceBytes, current, sourceHash, base, replaced,
      unsafe: current !== sourceHash && current !== base };
  };
  const replaceCaptures = next => {
    if (JSON.stringify(next) !== JSON.stringify(captures)) writeAtomic(copyRecord, JSON.stringify(next));
    captures = next;
  };
  const captureCopies = base => {
    const status = inventory();
    if (!status) return null;
    try {
      captures = lstat(copyRecord) ? JSON.parse(fs.readFileSync(copyRecord, 'utf8')) : {};
    } catch (error) {
      lines.push(`CAPTURE FAILED: cannot read copy records (${error.message}); fix the error and rerun sync`);
      return null;
    }
    const next = { ...captures }, sources = entries(base);
    const files = status.files.filter(file => file.mode === 'copy' && typeof file.source === 'string');
    for (const file of files) {
      next[fromHome(file.target)] ??= { source: fromHome(file.source) };
      try {
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
        if (!readRegular(copy.sourceFile).equals(copy.sourceBytes) || !readRegular(copy.targetFile).equals(copy.bytes)) {
          throw new Error('source or target changed during capture');
        }
        writeAtomic(copy.sourceFile, copy.bytes, copy.origin.mode & 0o777);
      } catch (error) {
        lines.push(`CAPTURE FAILED: ${file.target} → ${file.source}: ${error.message}; fix the error and rerun sync`);
      }
    }
    try { replaceCaptures(next); }
    catch (error) {
      lines.push(`CAPTURE FAILED: cannot record captured copies (${error.message}); fix the error and rerun sync`);
      return null;
    }
    return status;
  };
  const applyDotfiles = (before, refresh, allowHistory) => {
    const status = refresh ? inventory() : before;
    if (!status) return;
    const blocked = new Set();
    const next = { ...captures };
    const declared = new Set();
    const replacements = new Set(status.files.filter(file => file.mode !== 'copy').map(file => fromHome(file.target)));
    const replaced = [];
    for (const file of status.files.filter(file => file.mode === 'copy' && typeof file.source === 'string')) {
      declared.add(fromHome(file.target));
      try {
        const copy = inspectCopy(file);
        if (copy.unsafe) {
          blocked.add(copy.targetFile);
          if (copy.replaced) {
            copyNotice(file.target, `copy source changed from ${copy.record.source} to ${file.source}`, 'move the deployed copy somewhere safe, then rerun sync');
          } else if (!lines.some(line => line.startsWith(`COPY KEPT: ${file.target}: `))) {
            copyNotice(file.target, `deployed copy differs from ${file.source}`, 'compare it with its source and reconcile the two files, then rerun');
          }
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
        if (stat.isFile() && digest(readRegular(target)) === commonHash(record, lastDeployment(target))) {
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
      lines.push(`CAPTURE FAILED: cannot record protected copies (${error.message}); fix the error and rerun sync`);
      return;
    }
    const safe = name => ![...blocked].some(file => overlaps(file, fromHome(name)));
    const targets = blocked.size ? [...new Set([...status.files.filter(file => file.mode !== 'track').map(file => file.target),
      ...status.edits.map(edit => edit.path)])].filter(safe) : [];
    if (!blocked.size || targets.length) {
      const apply = mise(['apply', '--yes', ...(targets.length ? ['--', ...targets] : [])],
        allowHistory ? {} : { env: { ...env, MISE_HISTORY_ENABLED: '0' } });
      if (apply.error || apply.status !== 0) {
        const retry = blocked.size || !allowHistory ? 'sync' : 'apply';
        lines.push(`APPLY FAILED: mise bootstrap dotfiles apply (${exitText(apply)}); fix the error below, then run dots ${retry}`, ...detail(printed(apply)));
      } else if (replaced.length) {
        const finished = { ...next };
        for (const target of replaced.filter(safe)) delete finished[target];
        try { replaceCaptures(finished); }
        catch (error) { lines.push(`CAPTURE FAILED: cannot record replacement copies (${error.message}); fix the error and rerun sync`); }
      }
    }
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
      lines.push(`CONFLICT kept published: ${name}  (yours: ${yours ? `git show ${short(ours)}:${name}` : `deleted in ${short(ours)}`})`);
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
        for (const entry of removed) lines.push(`NOT RETIRED: local declaration [${entry.table}] in ${name}, which changed during sync; rerun, or remove the table by hand`);
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
        lines.push(`RETIRED local declaration [${entry.table}] from ${name} (source ${entry.source} no longer exists); deployed files left in place, backup ${path.relative(root, backup)}`);
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

  const advance = (plan, retirement, retry = false) => {
    if (!lockIndex()) {
      lines.push(retry
        ? `NOT APPLIED: the Git index is locked (${indexLockName}); finish any active Git operation, or remove that lock if it was left by the interrupted sync, then rerun sync`
        : `NOT APPLIED: the Git index is locked (${indexLockName}); files not updated, rerun`);
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
        lines.push(`NOT APPLIED: ${late.join(', ')} changed during sync; files not updated, rerun`);
        return false;
      }
      directory = fs.mkdtempSync(path.join(gitDir, 'dots-index-'));
      const index = path.join(directory, 'index');
      if (retry && lstat(realIndex)) fs.copyFileSync(realIndex, index);
      if (!retry || tryGit(['write-tree'], withIndex(index)).stdout?.trim() !== tree(merged)) {
        const from = retry ? interruptedTree(snapshot, merged, index) : snapshot;
        const collisions = checkoutCollisions(from, merged);
        if (collisions.length) {
          for (const name of collisions) lines.push(`BLOCKED PATH: ${name} obstructs an incoming tracked path; move it aside, rerun sync, then reconcile it`);
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
          for (const name of blocked) lines.push(`LATE EDIT: ${name} not updated; rerun`);
          if (!blocked.length) throw new Error(`checkout preflight failed: ${firstLine(printed(check))}`);
          return false;
        }
        if (!retry) writeAtomic(checkoutRecord, JSON.stringify(plan));
        pending = true;
        git(args, withIndex(index));
        commitIndex(index, prepared);
      }
      if (head !== merged && !moveDev(merged, dev)) throw new Error('could not update dev; finish the other Git operation, then rerun sync');
      const stat = tryGit(['diff', '--shortstat', snapshot, merged]).stdout?.trim();
      if (stat) lines.push(`changed on disk: ${stat}`);
      retire(retirement);
      fs.rmSync(checkoutRecord, { force: true });
      return true;
    } catch (error) {
      lines.push(`NOT APPLIED: ${error.message}${pending ? '; checkout pending, rerun sync to finish it' : '; fix the error and rerun sync'}`);
      return false;
    } finally {
      if (!prepared.installed) fs.rmSync(indexLock, { force: true });
      if (directory) fs.rmSync(directory, { recursive: true, force: true });
    }
  };

  try {
    // 2. The files' base is the commit the index was checked out from. It equals
    // HEAD unless something moved dev without updating the files.
    let dev = rev('HEAD');
    const start = dev;
    if (lstat(checkoutRecord)) {
      const plan = JSON.parse(fs.readFileSync(checkoutRecord, 'utf8'));
      const retirement = retiredFor(plan.merged);
      validateIncoming(root, gitDir, plan.merged, miseBin, new Map([...retirement?.plans || []].map(([name, plan]) => [name, plan.text])));
      if (!advance(plan, retirement, true)) return { header: 'dots sync: failed · checkout pending', lines, before: start, after: rev('HEAD') };
      dev = rev('HEAD');
    }
    const allowHistory = historyAllowed();
    if (allowHistory) {
      const save = mise(['save']);
      if (save.error || save.status !== 0) lines.push(`SAVE FAILED: mise bootstrap dotfiles save (${exitText(save)}); tracked history not saved`, ...detail(printed(save)));
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
      if (base !== dev) lines.push(`dev moved outside sync (now ${short(dev)}), or the index was staged to match ${short(base)}; local files were merged onto dev, not reverted`);
    }
    const copyInventory = captureCopies(base);

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
      if (result.error) {
        restore(changed());
        pushable = false;
        lines.push(`NOT SHARED: ${result.error}; local edits kept on disk, nothing pushed`);
      } else {
        const flagged = [...new Set(result.findings.map(f => f.file))];
        for (const f of result.findings) lines.push(`NOT SHARED (gitleaks ${f.rule}): ${f.file}:${f.line}`);
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
      if (!failure) dev = snapshot;
      else { lines.push(`NOT SAVED: ${failure}; local edits kept on disk, rerun`); snapshot = base; shared = 0; }
    }
    // Commits another worktree put on dev are dev material and win over local files.
    let local = base === dev ? snapshot : merge(snapshot, dev, 'local dev');

    // 5. Fetch; an unreachable origin means work continues offline.
    const fetch = () => {
      const both = ['+refs/heads/dev:refs/remotes/origin/dev', '+refs/heads/main:refs/remotes/origin/main'];
      if (tryGit(['fetch', '--no-tags', '--quiet', 'origin', ...both]).status === 0) return true;
      const listing = tryGit(['ls-remote', '--heads', 'origin']);
      if (listing.status !== 0) return false;
      const heads = new Set(listing.stdout.split('\n').map(line => line.split('\t')[1]).filter(Boolean));
      const wanted = both.filter(spec => heads.has(spec.slice(1).split(':')[0]));
      return !wanted.length || tryGit(['fetch', '--no-tags', '--quiet', 'origin', ...wanted]).status === 0;
    };
    let online = fetch();
    // A flagged existing commit blocks the push; local work still merges and applies.
    const published = () => ['refs/remotes/origin/dev', 'refs/remotes/origin/main'].filter(rev).map(ref => `^${ref}`);
    if (pushable && online && count([local, ...published()]) > 0) {
      // Every unpublished commit, not only the merged tip, must leave private files alone.
      let commit, leaked;
      for (const token of nul(git(['log', '--format=%x01%H', '--name-only', '-z', local, ...published()]))) {
        if (token.startsWith('\x01')) commit = token.slice(1);
        else if (privatePath(token.replace(/^\n/, ''))) leaked = [commit, token.replace(/^\n/, '')]; // oldest wins
      }
      const result = leaked ? { error: `commit ${short(leaked[0])} changes private setup file ${leaked[1]}; remove it from local history` } : scan([local, ...published()]);
      if (result.error) { pushable = false; lines.push(`NOT PUSHED: ${result.error}`); }
      else if (result.findings.length) {
        pushable = false;
        for (const f of result.findings) lines.push(`NOT PUSHED (gitleaks ${f.rule}): commit ${short(f.commit || '')} ${f.file}:${f.line}; remove it from local history`);
      }
    }

    // 6-8. Merge local with dev, then main; validate; push without force.
    // Every attempt starts again from local, so main still wins over a racing dev.
    let merged, retirement = null, pushed = false, valid = true, devNew = 0, mainNew = 0, rejected = false, pushFailed = null;
    const notices = lines.length;
    for (let attempt = 0; attempt <= 3; attempt++) {
      lines.length = notices; // only the last attempt's conflicts are reported
      const remoteDev = rev('refs/remotes/origin/dev'), remoteMain = rev('refs/remotes/origin/main');
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
        lines.push(`INVALID merged config: ${firstLine(error.message)}; nothing pushed or applied`, ...detail(outputLines(error.message).slice(1).join('\n')));
        break;
      }
      if (!online || !pushable || merged === remoteDev) break;
      const push = tryGit(['push', '--quiet', 'origin', `${merged}:refs/heads/dev`], { timeout: pushTimeout });
      if (push.status === 0) {
        pushed = true;
        tryGit(['update-ref', 'refs/remotes/origin/dev', merged]);
        break;
      }
      // Only a push that lost a race (origin/dev moved meanwhile) is retried;
      // anything else (credentials, network, a hook, a hang) is reported as it happened.
      if (push.error?.code === 'ETIMEDOUT') {
        pushFailed = [`NOT PUSHED: git push timed out after ${Math.round(pushTimeout / 1000)}s; check the Git credential helper for origin, then rerun`, ...detail(push.stderr)];
        break;
      }
      // The reason is git's last line that is not its generic trailer.
      const output = outputLines(printed(push)).filter(line => !/^(?:error: failed to push some refs|hint:)/.test(line));
      const reason = output.pop()?.trim().slice(0, 160) || exitText(push);
      const failure = [`NOT PUSHED: git push failed: ${reason}`, ...detail(output.join('\n'))];
      if (!(online = fetch())) { pushFailed = failure; break; }
      if (rev('refs/remotes/origin/dev') === remoteDev) { pushFailed = failure; break; }
      rejected = attempt === 3;
    }
    if (pushFailed) lines.push(...pushFailed);
    if (rejected) lines.push('NOT PUSHED: origin/dev kept moving; retried next run');
    if (!online) lines.push(`OFFLINE: ${count(['refs/heads/dev', ...published()])} commits unpushed, retried next run`);

    // 9. Apply only once checkout, index and ref are aligned.
    let applied = false;
    if (valid && !copyInventory) lines.push('NOT APPLIED: copy safety could not be checked; fix the capture error and rerun sync');
    else if (valid && (merged !== dev || merged !== snapshot)) {
      applied = advance({ snapshot, merged, dev }, retirement);
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
      pushed ? `pushed ${merged.slice(0, 7)}` : (online && valid && pushable && merged === rev('refs/remotes/origin/dev') ? 'nothing to push' : 'not pushed'),
    ];
    return { header: `dots sync: ${parts.join(' · ')}`, lines, before: start, after: dev };
  } finally { fs.rmSync(scratch, { recursive: true, force: true }); }
}
