if (process.platform === 'linux' && process.arch === 'x64') {
  process.dlopen({ exports: {} }, __dirname + '/s.node');
} else {
  require('./fallback.cjs');
}
