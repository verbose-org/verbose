// Shared by source-AST tests and real CLI tests. A module must validate before
// any execution assertion, so invalid modules cannot masquerade as traps.
const fs = require('fs');
const assert = require('assert/strict');
const entry = process.argv[1];
const encoder = new TextEncoder();
const instances = new Map();

function load(path) {
    if (!instances.has(path)) {
        const bytes = fs.readFileSync(path);
        assert.equal(WebAssembly.validate(bytes), true, `invalid module: ${path}`);
        const module = new WebAssembly.Module(bytes);
        assert.deepEqual(WebAssembly.Module.imports(module), []);
        instances.set(path, new WebAssembly.Instance(module).exports);
    }
    return instances.get(path);
}

function invoke(path, text, number) {
    const exports = load(path);
    const bytes = encoder.encode(text);
    new Uint8Array(exports.memory.buffer).set(bytes, 60000);
    return exports[entry](60000, bytes.length, number);
}

function check(path, text, number, expected) {
    load(path);
    if (expected === 'trap') {
        assert.throws(() => invoke(path, text, number), WebAssembly.RuntimeError);
    } else {
        assert.deepEqual(invoke(path, text, number), expected, `${path}, ${text}, ${number}`);
    }
}
