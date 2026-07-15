# Runtime manifests

Pass `--runtime-dir <directory>` to load every `.json` manifest in a trusted directory. A manifest with the same `id` replaces a built-in runtime; a new `id` adds a language without changing Rust code.

```bash
mkdir custom-runtimes
cp runtimes/lua.json.example custom-runtimes/lua.json
./target/release/aegis-core --runtime-dir custom-runtimes list
```

Supported placeholders:

- `{workspace}`
- `{source}`
- `{c_source}`
- `{cpp_source}`
- `{py_source}`
- `{js_source}`
- `{go_source}`
- `{binary}`

Each step may set `file_limit_bytes`; omitted values default to 2 MiB. Compiler steps that legitimately create larger temporary archives should opt into a narrowly sized higher limit.

Manifests are administrator configuration, not user input. Keep command vectors fixed and never expose manifest installation through the execution API.

The process backend resolves executables only from its fixed safe `PATH`. A Firecracker backend should interpret the same command vectors inside the selected runtime image.
