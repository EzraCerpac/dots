import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { privatePath } from './source-dev.mjs';

const receiptName = 'dots-sync-receipt.json';
const labels = {
  shared: 'Shared on dev',
  saved: 'Saved locally',
  received: 'Updated in this checkout',
  recovered: 'Finished pending checkout',
};
const operations = { A: 'added', C: 'copied', D: 'removed', M: 'edited', R: 'renamed', T: 'changed type' };
const oid = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/;
const controls = /[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/u;
const contentControls = /[^\P{Cc}\n\t]|[\p{Cf}\p{Zl}\p{Zp}]/u;
const encryptedPath = /(?:^|\/)(?:encrypted|\.sops[^/]*)(?:\/|$)|\.(?:age|asc|enc|gpg|pgp|sops)$/i;
const encryptedContent = /age-encryption\.org\/v1|-----BEGIN (?:AGE ENCRYPTED FILE|PGP MESSAGE|ENCRYPTED PRIVATE KEY)-----|ENC\[AES256_GCM,|^\s*sops\s*:/m;
const secretsPath = /^(?:\.env(?:\..*)?|\.netrc|\.git-credentials|id_(?:rsa|dsa|ecdsa|ed25519))$/i;
const decode = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });
const patchLimit = 64 * 1024;
const blobLimit = 256 * 1024;
const metadataLimit = 16 * 1024 * 1024;
const replayPatchLimit = 1024 * 1024;
const replayPatchTime = 3000;

function remainingTime(deadline, limit) {
  return Math.min(limit, Math.floor(deadline - performance.now()));
}

function git(root, args, { attrSource, deadline = Infinity, ...options } = {}) {
  const timeout = remainingTime(deadline, 5000);
  if (timeout <= 0) return null;
  const env = { ...process.env, GIT_NO_LAZY_FETCH: '1', GIT_NO_REPLACE_OBJECTS: '1', GIT_OPTIONAL_LOCKS: '0',
    GIT_ATTR_NOSYSTEM: '1', GIT_TERMINAL_PROMPT: '0', GIT_ALLOW_PROTOCOL: '', LC_ALL: 'C' };
  for (const name of ['GIT_DIR', 'GIT_COMMON_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE', 'GIT_NAMESPACE',
    'GIT_OBJECT_DIRECTORY', 'GIT_ALTERNATE_OBJECT_DIRECTORIES', 'GIT_EXTERNAL_DIFF', 'GIT_DIFF_OPTS', 'GIT_ATTR_SOURCE']) {
    delete env[name];
  }
  if (attrSource) env.GIT_ATTR_SOURCE = attrSource;
  const run = spawnSync('git', ['--no-pager', '-c', 'core.attributesFile=/dev/null', ...args], {
    cwd: path.resolve(root), env, timeout, killSignal: 'SIGKILL', maxBuffer: metadataLimit, ...options,
  });
  return run.status === 0 && !run.error ? run.stdout : null;
}

function gitDirectory(root) {
  if (git(root, ['rev-parse', '--show-prefix'], { encoding: 'utf8' }) !== '\n') return null;
  const result = git(root, ['rev-parse', '--absolute-git-dir'], { encoding: 'utf8' });
  if (!result) return null;
  const directory = fs.realpathSync(result.replace(/\n$/, ''));
  const relative = path.relative(fs.realpathSync(root), directory);
  // Never leave metadata in a directory that the next sync could publish.
  return relative === '.git' || relative.startsWith('..' + path.sep) ? directory : null;
}

function utf8(bytes) {
  try { return decode.decode(bytes); } catch { return null; }
}

const escapeControls = value => value.replace(/[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/gu, char =>
  '\\u' + char.codePointAt(0).toString(16).padStart(4, '0'));
const cleanText = value => escapeControls(String(value ?? '').trim()
  .replace(/([a-z][a-z0-9+.-]*:\/\/)[^\s/?#]+@/gi, '$1***@'));

function displayPath(raw) {
  const bytes = Buffer.from(raw, 'latin1'), name = utf8(bytes);
  if (name !== null) return /[\s"\\]/u.test(name) || controls.test(name) ? escapeControls(JSON.stringify(name)) : name;
  const escaped = Array.from(bytes, byte => byte >= 0x20 && byte <= 0x7e && byte !== 34 && byte !== 92
    ? String.fromCharCode(byte) : '\\' + byte.toString(8).padStart(3, '0')).join('');
  return '"' + escaped + '"';
}

function validRange({ before, after }) {
  return oid.test(before) && oid.test(after) && before.length === after.length;
}

function diff(root, { before, after }, args, { maxBuffer = metadataLimit, paths = [], deadline } = {}) {
  return git(root, ['--literal-pathspecs', '-c', 'core.quotePath=true', '-c', 'diff.suppressBlankEmpty=false',
    'diff', '--no-ext-diff', '--no-textconv', '--no-color',
    '--no-relative', '--ignore-submodules=none', '--diff-algorithm=myers', '--find-renames=50%', '-l1000',
    '--no-abbrev', ...args, before, after, '--', ...paths], { attrSource: after, maxBuffer, deadline });
}

// Both formats use NUL terminators for paths. Keep byte strings until the
// records are joined so even non-UTF-8 paths remain distinct.
function changedFiles(root, range, deadline) {
  if (!validRange(range)) return null;
  const raw = diff(root, range, ['--raw', '--numstat', '-z'], { deadline });
  if (!raw) return null;
  if (!raw.length) return [];
  const fields = raw.toString('latin1').split('\0');
  if (fields.pop() !== '') return null;
  const files = [];
  let index = 0;
  while (fields[index]?.startsWith(':')) {
    const record = /^:([0-7]{6}) ([0-7]{6}) ([0-9a-f]+) ([0-9a-f]+) ([ACDMRTUXB])\d*$/.exec(fields[index++]);
    if (!record) return null;
    const [, oldMode, newMode, oldBlob, newBlob, operation] = record;
    const renamed = operation === 'R' || operation === 'C';
    const from = renamed ? fields[index++] : null;
    const name = fields[index++];
    if (!name || (renamed && !from)) return null;
    files.push({ name, from, operation, oldMode, newMode, oldBlob, newBlob });
  }
  for (const file of files) {
    const stat = /^(\d+|-)\t(\d+|-)\t([\s\S]*)$/.exec(fields[index++] || '');
    if (!stat) return null;
    const [, added, removed, name] = stat;
    if (file.from ? name || fields[index++] !== file.from || fields[index++] !== file.name : name !== file.name) return null;
    file.added = added === '-' ? null : Number(added);
    file.removed = removed === '-' ? null : Number(removed);
  }
  return index === fields.length ? files : null;
}

function duringUp() {
  return Boolean(process.env.DOTS_UP_RUN_DIR || process.env.DOTS_UP_STREAM_NONCE || process.env.DOTS_UP_EVENT_FD);
}

function fileEntry({ name, from, operation, added, removed }) {
  return { name: displayPath(name), ...(from ? { from: displayPath(from) } : {}), operation, added, removed };
}

function recordedFiles(root, group, deadline) {
  if (!Array.isArray(group.files)) return null;
  const files = changedFiles(root, group, deadline);
  if (!files || files.length !== group.files.length) return null;
  return files.every((file, i) => {
    const entry = fileEntry(file), saved = group.files[i];
    return entry.name === saved.name && entry.from === saved.from && entry.operation === saved.operation &&
      entry.added === saved.added && entry.removed === saved.removed;
  }) ? files : null;
}

export function buildReceipt(root, { notices = [], issues = [], groups = [], source = 'sync', status, scanBlocked = false } = {}) {
  const normalizedIssues = issues.map(({ summary, detail, action }) => ({
    summary: cleanText(summary), detail: (Array.isArray(detail) ? detail : String(detail ?? '').split(/\r?\n/))
      .map(cleanText).filter(Boolean).join('\n'), action: cleanText(action),
  }));
  return {
    version: 1,
    source,
    finishedAt: new Date().toISOString(),
    status: status || (normalizedIssues.length ? 'attention' : 'complete'),
    issues: normalizedIssues,
    notices: notices.map(line => ({ text: cleanText(line), detail: [] })).filter(notice => notice.text),
    scanBlocked,
    groups: groups.map(({ kind, before, after }) => {
      const files = changedFiles(root, { before, after });
      return { kind, before, after, files: files?.map(fileEntry) ?? null };
    }),
  };
}

function issueLines({ summary, detail, action }) {
  return [summary, ...detail.split('\n').filter(Boolean).map(line => '  ' + line), ...(action ? ['  ' + action] : [])];
}

function formatFile({ name, from, operation, added, removed }) {
  return (operations[operation] || 'changed') + ' ' + (from ? from + ' → ' : '') + name +
    (added === null ? '' : ' (+' + added + ' −' + removed + ')');
}

function formatGroup({ kind, files }, limit) {
  const prefix = labels[kind];
  if (!files) return [prefix + ' · file list unavailable'];
  if (!files.length) return [prefix + ' · no file changes'];
  return [prefix + ' · ' + files.length + (files.length === 1 ? ' file' : ' files'),
    ...files.slice(0, limit).map(file => '  ' + formatFile(file)),
    ...(files.length > limit ? ['  … ' + (files.length - limit) + ' more'] : []),
  ];
}

export function receiptChangeRows(receipt) {
  return receipt.groups.flatMap(group => {
    const [heading, ...files] = formatGroup(group, 5);
    return files.length ? files.map(file => labels[group.kind] + ' · ' + file.trimStart()) : [heading];
  });
}

export function formatReceipt(receipt) {
  const lines = [
    ...receipt.issues.flatMap(issueLines),
    ...receipt.groups.flatMap(group => formatGroup(group, 5)),
    ...receipt.notices.map(({ text }) => text),
  ];
  if (!lines.length) lines.push('Sync is up to date.');
  if (receipt.replayUnavailable) lines.push('Could not save this sync for replay.');
  else if (receipt.groups.length || receipt.issues.some(issue => issue.detail) || receipt.notices.some(notice => notice.detail.length)) {
    lines.push('Run dots sync --last for the full receipt.');
  }
  return lines.join('\n');
}

// Pick the persisted fields explicitly. Patch content is derived on demand.
function metadata(receipt) {
  return {
    version: receipt.version, source: receipt.source, finishedAt: receipt.finishedAt,
    status: receipt.status, scanBlocked: receipt.scanBlocked,
    issues: receipt.issues.map(({ summary, detail, action }) => ({ summary, detail, action })),
    notices: receipt.notices.map(({ text, detail }) => ({ text, detail })),
    groups: receipt.groups.map(({ kind, before, after, files }) => ({
      kind, before, after, files: files?.map(({ name, from, operation, added, removed }) =>
        ({ name, from, operation, added, removed })) ?? null,
    })),
  };
}

export function saveReceipt(root, receipt) {
  const gitDir = gitDirectory(root);
  if (!gitDir) return false;
  const stored = metadata(receipt);
  let json = JSON.stringify(stored) + '\n';
  if (Buffer.byteLength(json) > metadataLimit) {
    for (const group of stored.groups) group.files = null;
    json = JSON.stringify(stored) + '\n';
  }
  if (Buffer.byteLength(json) > metadataLimit) throw new Error('Sync receipt metadata is too large');
  const directory = fs.mkdtempSync(path.join(gitDir, '.dots-receipt-'));
  try {
    const pending = path.join(directory, receiptName);
    fs.writeFileSync(pending, json, { mode: 0o600 });
    fs.renameSync(pending, path.join(gitDir, receiptName));
  } finally { fs.rmSync(directory, { recursive: true, force: true }); }
  return true;
}

export function readReceiptFile(file) {
  let fd;
  try {
    fd = fs.openSync(file, fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW | fs.constants.O_NONBLOCK);
    const stat = fs.fstatSync(fd);
    if (!stat.isFile() || stat.size > metadataLimit) throw new Error('invalid receipt');
    const receipt = JSON.parse(fs.readFileSync(fd, 'utf8'));
    const text = value => typeof value === 'string' && !controls.test(value);
    const multiline = value => typeof value === 'string' && value.split('\n').every(text);
    const count = value => value === null || (Number.isSafeInteger(value) && value >= 0);
    const array = (value, check) => Array.isArray(value) && value.every(check);
    const older = receipt.version === 0 || (receipt.version === 1 && receipt.scanBlocked === undefined);
    if (older) receipt.scanBlocked = true;
    if ((!older && receipt.version !== 1) || !['up', 'sync'].includes(receipt.source) || !text(receipt.finishedAt) ||
      !['complete', 'attention', 'failed', 'skipped', 'stopped'].includes(receipt.status) || typeof receipt.scanBlocked !== 'boolean' ||
      !array(receipt.issues, issue => issue && text(issue.summary) && multiline(issue.detail) && text(issue.action)) ||
      !array(receipt.notices, notice => notice && text(notice.text) && array(notice.detail, text)) ||
      !array(receipt.groups, group => group && Object.hasOwn(labels, group.kind) && validRange(group) &&
        (group.files === null || array(group.files, file => file && text(file.name) &&
          (file.from === undefined || text(file.from)) && /^[ACDMRTUXB]$/.test(file.operation) &&
          count(file.added) && count(file.removed))))) throw new Error('invalid receipt');
    if (older) receipt.notices.push({ text: 'Patch unavailable for this older sync receipt.', detail: [] });
    return receipt;
  } catch (error) {
    if (error.code === 'ENOENT') return null;
    throw error;
  } finally {
    if (fd !== undefined) fs.closeSync(fd);
  }
}

function displayablePath(raw) {
  const name = utf8(Buffer.from(raw, 'latin1'));
  if (name === null || controls.test(name) || encryptedPath.test(name) ||
    name.split('/').some(component => !component || component === '.' || component === '..' ||
      privatePath(component) || secretsPath.test(component))) return null;
  return name;
}

function scan(mode, gitleaks, { cwd, name, input, deadline }) {
  if (typeof gitleaks !== 'string' || !gitleaks || gitleaks.includes('\0')) return false;
  const timeout = remainingTime(deadline, 10000);
  if (timeout <= 0) return false;
  // Use stock rules regardless of repository allowlists and in-file comments.
  const env = { ...process.env, GITLEAKS_CONFIG_TOML: '[extend]\nuseDefault = true' };
  delete env.GITLEAKS_CONFIG;
  const flags = ['--no-banner', '--no-color', '--redact', '--log-level', 'warn', '--ignore-gitleaks-allow',
    '--gitleaks-ignore-path', '/dev/null', '--report-format', 'json', '--report-path', '-'];
  const result = spawnSync(gitleaks, [mode, ...flags, ...(name ? ['--', name] : [])], {
    env, cwd, input, encoding: 'utf8', timeout, killSignal: 'SIGKILL', maxBuffer: patchLimit,
  });
  // Older scanners that write no report leave the receipt metadata-only.
  return result.status === 0 && !result.error && !result.stderr.trim() && result.stdout.trim() === '[]';
}

function scanBlob(root, name, blob, gitleaks, deadline) {
  if (/^0+$/.test(blob)) return true;
  if (!oid.test(blob)) return false;
  const bytes = git(root, ['cat-file', 'blob', blob], { maxBuffer: blobLimit, deadline });
  if (!bytes || bytes.length > blobLimit) return false;
  const content = utf8(bytes);
  if (content === null || contentControls.test(content) || encryptedContent.test(content)) return false;
  if (!bytes.length) return true;

  // Stdin has no path, which skips rules tied to filenames and extensions.
  // Give the scanner the original relative path in private temporary storage.
  let scratch;
  try {
    scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'dots-receipt-scan-'));
    const target = path.join(scratch, name);
    fs.mkdirSync(path.dirname(target), { recursive: true, mode: 0o700 });
    fs.writeFileSync(target, bytes, { mode: 0o600, flag: 'wx' });
    return scan('dir', gitleaks, { cwd: scratch, name, deadline });
  } catch {
    return false;
  } finally {
    if (scratch) fs.rmSync(scratch, { recursive: true, force: true });
  }
}

function safeFilePatch(root, group, file, { gitleaks = process.env.DOTS_RECEIPT_GITLEAKS ?? 'gitleaks', width = Infinity,
  linesLeft = Infinity, bytesLeft = patchLimit, deadline = Infinity } = {}) {
  if (file.added === null || file.removed === null ||
    [file.oldMode, file.newMode].some(mode => !['100644', '100755', '000000'].includes(mode))) return '';
  const name = displayablePath(file.name), from = file.from ? displayablePath(file.from) : name;
  if (!name || !from) return '';
  const raw = diff(root, group, ['--patch', '--full-index', '--unified=3', '--no-indent-heuristic', '--inter-hunk-context=0',
    '--src-prefix=a/', '--dst-prefix=b/', '--output-indicator-new=+', '--output-indicator-old=-',
    '--output-indicator-context= '], { maxBuffer: patchLimit, paths: from === name ? [name] : [from, name], deadline });
  const patch = raw && utf8(raw);
  if (!patch || contentControls.test(patch) || encryptedContent.test(patch)) return '';

  const heading = '  ' + formatFile({ ...file, name: displayPath(file.name), from: file.from && displayPath(file.from) });
  const lines = [heading];
  let inHunk = false, sections = 0, added = 0, removed = 0;
  let verifiedIndex = file.oldBlob === file.newBlob;
  for (const line of patch.replace(/\n$/, '').split('\n')) {
    if (line.startsWith('diff --git ')) { sections++; inHunk = false; }
    else if (line.startsWith('@@ ')) {
      if (inHunk) lines.push('    …');
      inHunk = true;
    }
    else if (inHunk && (/^[ +\-]/.test(line) || line === '\\ No newline at end of file')) {
      if (line[0] === '+') added++;
      if (line[0] === '-') removed++;
      lines.push('    ' + line);
    } else if (line.startsWith('index ')) {
      verifiedIndex = line.split(' ')[1] === file.oldBlob + '..' + file.newBlob;
    } else if (inHunk || line.startsWith('Binary files ') || line === 'GIT binary patch') return '';
  }
  const display = lines.join('\n') + '\n';
  if (sections !== 1 || !verifiedIndex || added !== file.added || removed !== file.removed ||
    lines.length > linesLeft || lines.some(line => displayWidth(line) > width) || Buffer.byteLength(display) > bytesLeft) return '';
  if (!scan('stdin', gitleaks, { cwd: '/', input: display, deadline }) ||
    !scanBlob(root, from, file.oldBlob, gitleaks, deadline) ||
    ((from !== name || file.oldBlob !== file.newBlob) && !scanBlob(root, name, file.newBlob, gitleaks, deadline))) return '';
  return display;
}

function displayWidth(line) {
  // UTF-8 byte length bounds terminal cell width, including wide characters.
  const [first, ...rest] = line.split('\t');
  return rest.reduce((width, part) => width + 8 - width % 8 + Buffer.byteLength(part), Buffer.byteLength(first));
}

export function inlinePatch(root, receipt, { gitleaks, width = process.stdout.columns || 80 } = {}) {
  if (receipt.status !== 'complete' || receipt.scanBlocked || receipt.source === 'up' ||
    duringUp() || !Number.isFinite(width) || width < 1) return '';
  let filesLeft = 2, linesLeft = 12, bytesLeft = 4096;
  const display = [];
  for (const group of receipt.groups) {
    const files = recordedFiles(root, group);
    if (!files || files.length > filesLeft) return '';
    if (!files.length) continue;

    const heading = labels[group.kind] + '\n';
    if (displayWidth(labels[group.kind]) > width) return '';
    display.push(heading);
    linesLeft--;
    bytesLeft -= Buffer.byteLength(heading);
    for (const file of files) {
      const patch = safeFilePatch(root, group, file, { gitleaks, width, linesLeft, bytesLeft });
      if (!patch) return '';
      display.push(patch);
      linesLeft -= patch.split('\n').length - 1;
      bytesLeft -= Buffer.byteLength(patch);
    }
    filesLeft -= files.length;
  }
  return display.join('');
}

function hasTree(root, commit) {
  return git(root, ['cat-file', '-t', commit + '^{tree}'], { encoding: 'utf8' }) === 'tree\n';
}

export function showLast(root, { plain = false } = {}) {
  let receipt;
  try {
    const gitDir = gitDirectory(root);
    if (gitDir) receipt = readReceiptFile(path.join(gitDir, receiptName));
  }
  catch { console.error('The latest sync receipt could not be read.'); return 1; }
  if (!receipt) { console.error('No sync receipt for this worktree.'); return 1; }

  const details = [
    ...receipt.issues.flatMap(issueLines),
    ...receipt.notices.flatMap(({ text, detail }) => [text, ...detail.map(line => '  ' + line)]),
    'Latest sync · ' + receipt.finishedAt + (receipt.source === 'up' ? ' · during dots up' : ' · dots sync'),
  ];
  if (receipt.status !== 'complete') details[details.length - 1] += ' · ' + receipt.status;
  if (!receipt.issues.length && !receipt.notices.length && !receipt.groups.length) details.push('Sync is up to date.');
  const patches = [];
  const interactive = !plain && process.stdin.isTTY && process.stdout.isTTY && !duringUp();
  const deadline = performance.now() + replayPatchTime;
  let bytesLeft = replayPatchLimit;
  for (const group of receipt.groups) {
    details.push('');
    for (const line of formatGroup(group, Infinity)) details.push(line);
    details.push('  ' + group.before + ' → ' + group.after);
    if (![group.before, group.after].every(commit => hasTree(root, commit))) {
      details.push('  Patch unavailable. The recorded Git objects may have been pruned.');
    } else if (interactive && !group.files) {
      details.push('  Patch unavailable. The recorded files could not be verified.');
    } else if (interactive && group.files.length) {
      const heading = labels[group.kind] + '\n';
      const headingBytes = Buffer.byteLength(heading) + (patches.length ? 1 : 0);
      let shown = 0;
      if (!receipt.scanBlocked && bytesLeft > headingBytes && performance.now() < deadline) {
        const files = recordedFiles(root, group, deadline);
        if (!files && performance.now() < deadline) {
          details.push('  Patch unavailable. The recorded files could not be verified.');
          continue;
        }
        for (const file of files ?? []) {
          const available = bytesLeft - (shown ? 0 : headingBytes);
          if (available <= 0 || performance.now() >= deadline) break;
          const patch = safeFilePatch(root, group, file, { bytesLeft: Math.min(available, patchLimit), deadline });
          if (!patch) continue;
          if (shown++ === 0) {
            patches.push(patches.length ? '\n' + heading : heading);
            bytesLeft -= headingBytes;
          }
          patches.push(patch);
          bytesLeft -= Buffer.byteLength(patch);
        }
      }
      if (shown < group.files.length) {
        const omitted = group.files.length - shown;
        details.push('  Patch omitted for ' + omitted + (omitted === 1 ? ' file.' : ' files.'));
      }
    }
  }
  const summary = details.join('\n') + '\n';
  if (interactive) {
    const env = Object.fromEntries(Object.entries(process.env).filter(([name]) => !name.startsWith('LESS')));
    Object.assign(env, { LESSSECURE: '1', LESSHISTFILE: '-' });
    const pager = spawnSync('less', ['-S', '--', '-'], {
      input: patches.length ? summary + '\n' + patches.join('') : summary, stdio: ['pipe', 'inherit', 'inherit'], env,
    });
    if (pager.status === 0) return 0;
  }
  process.stdout.write(summary);
  return 0;
}
