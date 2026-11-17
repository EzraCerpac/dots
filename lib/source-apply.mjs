import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

export const shellQuote = value => "'" + value.replaceAll("'", "'\\''") + "'";
export const fromHome = name => path.resolve(name.startsWith('~/') ? path.join(os.homedir(), name.slice(2)) : name);

export function runMise(bin, root, args, env = process.env) {
  return spawnSync(bin, ['-C', root, ...args], { cwd: root, env, encoding: 'utf8', timeout: 60000, maxBuffer: 64 * 1024 * 1024 });
}

function readJson(result, description) {
  if (result.error || result.status !== 0) throw new Error(`${description} (${result.error?.message || result.stderr.trim()})`);
  return JSON.parse(result.stdout);
}

// Use mise for TOML decoding and directory expansion. A private config root
// ensures these read/materialize commands cannot apply the user's dotfiles,
// execute hooks, or add their temporary targets to the user's history.
function isolatedMise(bin, scratch, config, args) {
  const dir = fs.mkdtempSync(path.join(scratch, 'native-'));
  try {
    fs.writeFileSync(path.join(dir, 'config.toml'), config, { mode: 0o600 });
    const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !/^(?:__)?MISE_/.test(key)));
    return runMise(bin, dir, args, { ...env, HOME: dir, MISE_CONFIG_DIR: dir, MISE_SYSTEM_CONFIG_DIR: path.join(dir, 'system'),
      MISE_DATA_DIR: path.join(dir, 'data'), MISE_STATE_DIR: path.join(dir, 'state'), MISE_CACHE_DIR: path.join(dir, 'cache'),
      MISE_CEILING_PATHS: path.dirname(dir), MISE_TRUSTED_CONFIG_PATHS: dir,
      MISE_HISTORY_ENABLED: '0', MISE_NO_HOOKS: '1', MISE_AUTO_INSTALL: '0', MISE_AUTO_UPDATE: '0' });
  } finally { fs.rmSync(dir, { recursive: true, force: true }); }
}

export function materializeDirectory(bin, scratch, target, source, options) {
  const fields = ['exclude', 'manifest', 'dot_prefix', 'permissions'].filter(key => options[key] !== undefined);
  const entry = ['mode = "copy"', `source = ${JSON.stringify(source)}`, ...fields.map(key => `${key} = ${JSON.stringify(options[key])}`)];
  const result = isolatedMise(bin, scratch, `[dotfiles]\n${JSON.stringify(target)} = { ${entry.join(', ')} }\n`,
    ['bootstrap', 'dotfiles', 'apply', '--yes']);
  if (result.error || result.status !== 0) throw new Error(result.error?.message || result.stderr.trim());
}

function readConfigs(bin, scratch, files) {
  if (!files.length) return new Map();
  const vars = files.map((file, index) => {
    // `mise vars` expands shell variables in its rendered values. Encode
    // the JSON so reload commands still get their own $HOME at execution.
    const expression = `{{ read_toml(path=${JSON.stringify(file)}) | json_encode | b64_encode }}`;
    return `config${index} = ${JSON.stringify(expression)}`;
  });
  const data = readJson(isolatedMise(bin, scratch, `[vars]\n${vars.join('\n')}\n`, ['vars', '--json']), 'cannot read mise apply configuration');
  return new Map(files.map((file, index) => [file, JSON.parse(Buffer.from(data[`config${index}`], 'base64').toString('utf8'))]));
}

export function applyMetadata(bin, scratch, files, env, withReloads = true) {
  // In an empty directory, config discovery lists the system/global layers
  // without project-only files. JSON is in priority order, highest first.
  const global = withReloads ? readJson(runMise(bin, scratch, ['config', 'ls', '--json'], { ...env, MISE_CEILING_PATHS: scratch }),
    'cannot list global mise configuration').filter(file => !file.override && path.extname(file.path) === '.toml')
    .map(file => file.path).reverse() : [];
  const configs = readConfigs(bin, scratch, [...new Set([...global, ...files.map(file => file.origin?.config)
    .filter(file => typeof file === 'string' && path.extname(file) === '.toml')])]);
  const reload = new Map();
  for (const file of global) {
    const rules = configs.get(file)?.history?.reload;
    if (!rules || !Object.keys(rules).length) continue;
    const schema = readJson(runMise(bin, scratch, ['config', 'explain', '--json', '--file', file, 'history.reload'], env),
      'cannot read mise reload order');
    for (const { path: key } of schema) if (key.length === 3 && Object.hasOwn(rules, key[2])) reload.set(key[2], rules[key[2]]);
  }
  const entries = new Map();
  for (const file of files) {
    const config = configs.get(file.origin?.config);
    if (!config) continue;
    const target = fromHome(file.target);
    const groups = Object.entries(config.dotfile_groups ?? {});
    const declarations = [
      ...Object.entries(config.dotfiles ?? {}).map(([name, opts]) => ({ name, opts })),
      ...groups.flatMap(([group, opts]) => Object.entries(opts.entries ?? {}).map(([name, opts]) => ({ name, opts, group }))),
    ];
    const matches = declarations.filter(({ name, opts }) => fromHome(name) === target || opts?.variants?.some(variant =>
      typeof variant.target === 'string' && fromHome(variant.target) === target));
    const direct = matches.length === 1 ? matches[0] : null;
    const group = groups.find(([, opts]) => typeof opts.target === 'string' && fromHome(opts.target) === target);
    entries.set(target, direct ?? (group && { name: file.target, opts: group[1], group: group[0] }));
  }
  return { entries, reload };
}

export function dependencyLayers(files, entries, blocked) {
  const names = new Map(files.map(file => [fromHome(file.target), file]));
  const dependencies = new Map();
  for (const target of names.keys()) {
    const entry = entries.get(target);
    const waitsFor = new Set();
    for (const dependency of entry?.opts?.depends ?? []) {
      const destination = fromHome(dependency);
      if (names.has(destination) || blocked.has(destination)) waitsFor.add(destination);
      for (const [name, other] of entries) {
        if (name !== target && (other?.group === dependency || (other?.name && fromHome(other.name) === destination))) waitsFor.add(name);
      }
    }
    dependencies.set(target, waitsFor);
  }
  const layers = [];
  const pending = new Set(names.keys());
  while (pending.size) {
    const ready = [...pending].filter(target => ![...dependencies.get(target)].some(dep => pending.has(dep)));
    if (!ready.length) throw new Error(`dotfile dependency cycle involving ${[...pending].join(', ')}`);
    layers.push(ready.map(target => ({ file: names.get(target), wait: [...dependencies.get(target)] })));
    for (const target of ready) pending.delete(target);
  }
  return layers;
}

export function queueReloads(bin, scratch, root, env, reload, override) {
  if (!reload.size) return () => {};
  const marker = path.join(scratch, 'reloads');
  const entries = [...reload];
  const rules = patterns => {
    // A home-relative glob and an absolute glob can map to the same mirror
    // path. Preserve both rule IDs in a single TOML entry.
    const matches = new Map();
    for (const [index, glob] of patterns.entries()) matches.set(glob, [...(matches.get(glob) ?? []), index]);
    return '[history.reload]\n' + [...matches].map(([glob, ids]) =>
      `${JSON.stringify(glob)} = ${JSON.stringify(`printf '%s\\n' ${ids.join(' ')} >> ${shellQuote(marker)}`)}`).join('\n') + '\n';
  };
  fs.writeFileSync(override, rules(entries.map(([glob]) => glob)), { flag: 'wx', mode: 0o600 });
  const loaded = readJson(runMise(bin, scratch, ['config', 'ls', '--json'], { ...env, MISE_CEILING_PATHS: scratch }),
    'cannot check the global mise reload override');
  if (!loaded.some(file => file.path === override && !file.override)) throw new Error('mise did not load the guarded reload profile');
  return (written, lines) => {
    if (written.length) {
      // Native apply reports its own writes via the overrides above. For
      // guarded writes, ask native reload to match empty scratch stand-ins
      // using the same paths and globs under one private prefix.
      const mirror = path.join(scratch, 'written');
      const prefix = mirror.replace(/[\\?*\[\]{}]/g, '\\$&');
      const home = os.homedir(), realHome = fs.realpathSync(home);
      const paths = [...new Set(written.map(file => file.startsWith(home + path.sep) ? realHome + file.slice(home.length) : file))];
      const files = paths.map(file => `${JSON.stringify(mirror + file)} = { content = "" }`);
      const patterns = entries.map(([glob]) => prefix + (glob.startsWith('~/') ? realHome + glob.slice(1) : glob));
      const result = isolatedMise(bin, scratch, `${rules(patterns)}\n[dotfiles]\n${files.join('\n')}\n`, ['bootstrap', 'dotfiles', 'apply', '--yes']);
      if (result.error || result.status !== 0) lines.push('RELOAD FAILED: mise could not match guarded copy writes; check [history.reload] and run the matching reload commands');
    }
    let triggered;
    try { triggered = new Set(fs.readFileSync(marker, 'utf8').trim().split('\n').filter(Boolean).map(Number)); }
    catch (error) { if (error.code !== 'ENOENT') throw error; triggered = new Set(); }
    const ran = new Set();
    for (const [id, [glob, command]] of entries.entries()) {
      if (!triggered.has(id) || ran.has(command)) continue;
      ran.add(command);
      const result = spawnSync('sh', ['-c', command], { cwd: root, env, timeout: 60000, stdio: 'ignore' });
      if (result.error || result.status !== 0) {
        lines.push(`RELOAD FAILED: [history.reload] ${glob} (${result.error?.code || `exit ${result.status}`}); check its command and retry it`);
      }
    }
  };
}
