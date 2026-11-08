// G2: CPython 3.12 as a WASI reactor (SPEC §6).
//
// The host spawns a root, calls `py_init` once, then runs statements with
// `py_exec`, so it can fork between statements with variables already
// defined. Strings are passed by `py_alloc`ing guest memory, writing into it
// from the host, and passing (ptr, len).

#include <Python.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#define EXPORT(name) __attribute__((export_name(#name)))

EXPORT(py_alloc) void *py_alloc(uint32_t len) { return malloc(len ? len : 1); }

EXPORT(py_free) void py_free(void *p) { free(p); }

// Initialises the interpreter. The stdlib is expected under /usr/local/lib
// (python312.zip plus the os.py landmark). Returns 0 on success.
EXPORT(py_init) int32_t py_init(void) {
    PyConfig config;
    PyConfig_InitIsolatedConfig(&config);
    config.site_import = 0;
    config.write_bytecode = 0;
    config.buffered_stdio = 0;
    config.install_signal_handlers = 0;
    config.use_hash_seed = 1;
    config.hash_seed = 0;
    PyStatus status = PyConfig_SetString(&config, &config.home, L"/usr/local");
    if (!PyStatus_Exception(status)) status = Py_InitializeFromConfig(&config);
    PyConfig_Clear(&config);
    return PyStatus_Exception(status) ? -1 : 0;
}

// Runs `len` bytes of source in __main__. Returns 0, or -1 if it raised (the
// traceback goes to stderr).
EXPORT(py_exec) int32_t py_exec(const char *src, uint32_t len) {
    char *code = malloc(len + 1);
    if (!code) return -1;
    memcpy(code, src, len);
    code[len] = 0;
    int rc = PyRun_SimpleString(code);
    free(code);
    return rc;
}

// Reads the integer variable `name` from __main__ (for scoring branches).
// Returns INT64_MIN if it is missing or not an int.
EXPORT(py_get_int) int64_t py_get_int(const char *name, uint32_t len) {
    PyObject *main = PyImport_AddModule("__main__"); // borrowed
    if (!main) return INT64_MIN;
    PyObject *key = PyUnicode_FromStringAndSize(name, len);
    if (!key) {
        PyErr_Clear();
        return INT64_MIN;
    }
    PyObject *v = PyDict_GetItemWithError(PyModule_GetDict(main), key); // borrowed
    Py_DECREF(key);
    int64_t out = INT64_MIN;
    if (v && PyLong_Check(v)) {
        out = PyLong_AsLongLong(v);
        if (out == -1 && PyErr_Occurred()) out = INT64_MIN;
    }
    PyErr_Clear();
    return out;
}
