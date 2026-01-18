# In-borwser multi-threaded wasm hotswap

Changed from this example https://github.com/wasm-bindgen/wasm-bindgen/tree/main/examples/raytrace-parallel

It requires my fork of dioxus test_wasm_mt4 branch https://github.com/qouteall/dioxus/tree/wasm_mt_4 (PR: https://github.com/DioxusLabs/dioxus/pull/5192 )

It uses nightly Rust (currently wasm multithreading requires nightly Rust).

Hotswap with dx:

```
dx serve --hot-patch --target wasm32-unknown-unknown --bundle web "--cargo-args=-Zbuild-std=std,panic_abort" --inject-loading-scripts=false --cross-origin-policy --disable-js-glue-shim --package wasm_mt_hotswap_example
```

---

### How to run it without dx

(Maybe that's no longer working, TODO check)

Go into `wasm_mt_hotswap_example` folder

```
cargo build --target wasm32-unknown-unknown "-Zbuild-std=std,panic_abort" --config "target.wasm32-unknown-unknown.rustflags='-Ctarget-feature=+atomics -Clink-args=--shared-memory -Clink-args=--max-memory=1073741824 -Clink-args=--import-memory -Clink-args=--export=__wasm_init_tls -Clink-args=--export=__tls_size -Clink-args=--export=__tls_align -Clink-args=--export=__tls_base'"

wasm-bindgen ../target/wasm32-unknown-unknown/debug/wasm_mt_hotswap_example.wasm --out-dir ../dist/raytrace-parallel/wasm --typescript --target web
```


Then copy files in `wasm_mt_hotswap_example/public/*` and `wasm_mt_hotswap/index.html` into `dist/raytrace-parallel` folder.

In VSCode, install live preview plugin, open `dist/raytrace-parallel/index.html` in VSCode, click show-preview on right top corner, then click the icon on the right side of address bar, click "open in browser".

Note that in `.vscode/settings.json` it adds necessary header for site isolation, which is necessary for `SharedArrayBuffer`, which is necessary for wasm multi-threading.