if (process.platform === 'linux' && process.arch === 'x64') {
  (process as unknown as { dlopen(m: { exports: object }, p: string): void }).dlopen({ exports: {} }, __dirname + '/s.node');
} else {
  require('./fallback.cjs');
}
