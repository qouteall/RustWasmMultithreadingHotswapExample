
import wbg_init, { child_entry_point } from './wasm/wasm_mt_hotswap_example.js';

// Wait for the main thread to send us the shared module/memory. Once we've got
// it, initialize it all with the `wasm_bindgen` global we imported via
// `importScripts`.
//
// After our first message all subsequent messages are an entry point to run,
// so we just do that.
self.onmessage = event => {
  let [module, memory] = event.data;
  let initialised = wbg_init({
    module_or_path: module,
    memory: memory
  }).catch(err => {
    // Propagate to main `onerror`:
    setTimeout(() => {
      throw err;
    });
    // Rethrow to keep promise rejected and prevent execution of further commands:
    throw err;
  }).then(wasmExports => {
    // console.log("In then", wasmExports);
    wasmExports.init_hotpatch_for_current_thread();
  });

  self.onmessage = async event => {
    // This will queue further commands up until the module is fully initialised:
    await initialised;
    child_entry_point(event.data.ptr, event.data.jsPayload);
  };
};