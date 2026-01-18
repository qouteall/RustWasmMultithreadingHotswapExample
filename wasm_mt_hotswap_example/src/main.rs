use anyhow::{bail, Context};
use dioxus_devtools::DevserverMsg;
use futures_channel::oneshot;
use js_sys::WebAssembly::Module;
use js_sys::{ArrayBuffer, JsString, Reflect, Uint8Array};
use js_sys::{
    Object, Promise, SharedArrayBuffer, Uint8ClampedArray,
    WebAssembly::{self, Memory, Table},
};
use manganis::{asset, Asset};
use rayon::prelude::*;
use std::io::Read;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::{io, mem};
use subsecond::{JumpTable, PatchError};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{console, ImageData, Window, WorkerGlobalScope};
use web_sys::{MessageEvent, WebSocket};

use crate::pool::{
    broadcast_to_workers, broadcast_to_workers_with_js, pool_get_web_worker_num, submit_to_pool,
};

static TEST_CSS_ASSET: Asset = asset!("/assets/test.css");

fn main() {
    // this is just placeholder
    // it won't be called when a #[wasm_bindgen(start)] function exists, because
    // https://github.com/DioxusLabs/dioxus/blob/f7e102a0b4868f51f35059ddacb19d78f10f0fa6/packages/cli/src/build/request.rs#L4242
    // dioxus doesn't work with lib target, so we need to pretend this lib is a bin
}

macro_rules! console_log {
    ($($t:tt)*) => (crate::log(&format_args!($($t)*).to_string()))
}

mod pool;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console)]
    fn log(s: &str);
    #[wasm_bindgen(js_namespace = console, js_name = log)]
    fn logv(x: &JsValue);
}

#[wasm_bindgen]
pub struct Scene {
    inner: raytracer::scene::Scene,
}

static NEXT_THREAD_ID: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    // for testing TLS
    static MY_THREAD_ID: usize = (NEXT_THREAD_ID.fetch_add(1, Ordering::Relaxed));
}

#[wasm_bindgen]
impl Scene {
    /// Creates a new scene from the JSON description in `object`, which we
    /// deserialize here into an actual scene.
    #[wasm_bindgen(constructor)]
    pub fn new(object: JsValue) -> Result<Scene, JsValue> {
        Ok(Scene {
            inner: serde_wasm_bindgen::from_value(object)
                .map_err(|e| JsValue::from(e.to_string()))?,
        })
    }

    /// Renders this scene with the provided concurrency and worker pool.
    ///
    /// This will spawn up to `concurrency` workers which are loaded from or
    /// spawned into `pool`. The `RenderingScene` state contains information to
    /// get notifications when the render has completed.
    pub fn render(self, concurrency: usize) -> Result<RenderingScene, JsValue> {
        let scene = self.inner;
        let height = scene.height;
        let width = scene.width;

        // Allocate the pixel data which our threads will be writing into.
        let pixels = (width * height) as usize;
        let mut rgb_data = vec![0; 4 * pixels];
        let base = rgb_data.as_ptr() as usize;
        let len = rgb_data.len();

        // Configure a rayon thread pool which will pull web workers from
        // `pool`.
        let thread_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(concurrency)
            .spawn_handler(|thread| {
                submit_to_pool(|| thread.run());
                // Update: seems that it must spawn new threads, cannot queue task
                // otherwise parallelism is not enough, rayon will stuck inside
                Ok(())
            })
            .build()
            .unwrap();

        // And now execute the render! The entire render happens on our worker
        // threads so we don't lock up the main thread, so we ship off a thread
        // which actually does the whole rayon business. When our returned
        // future is resolved we can pull out the final version of the image.
        let (tx, rx) = oneshot::channel();
        submit_to_pool(move || {
            thread_pool.install(|| {
                subsecond::call(|| {
                    rgb_data
                        .par_chunks_mut(4)
                        .enumerate()
                        .for_each(|(i, chunk)| {
                            let i = i as u32;
                            let x = i % width;
                            let y = i / width;
                            let ray = raytracer::Ray::create_prime(x, y, &scene);
                            let result = raytracer::cast_ray(&scene, &ray, 0).to_rgba();
                            chunk[0] = result.data[0];
                            // chunk[0] = 255u8;
                            chunk[1] = result.data[1];
                            chunk[2] = result.data[2];
                            chunk[3] = result.data[3];

                            // if MY_THREAD_ID.with(|s|*s) % 8 == 0 {
                            //     chunk[0] = 255u8;
                            // }
                        });
                })
            });
            drop(tx.send(rgb_data));
        })?;

        let done = async move {
            match rx.await {
                Ok(_data) => Ok(image_data(base, len, width, height).into()),
                Err(_) => Err(JsValue::undefined()),
            }
        };

        Ok(RenderingScene {
            promise: wasm_bindgen_futures::future_to_promise(done),
            base,
            len,
            height,
            width,
        })
    }
}

#[wasm_bindgen]
pub struct RenderingScene {
    base: usize,
    len: usize,
    promise: Promise,
    width: u32,
    height: u32,
}

#[wasm_bindgen]
impl RenderingScene {
    /// Returns the JS promise object which resolves when the render is complete
    pub fn promise(&self) -> Promise {
        self.promise.clone()
    }

    /// Return a progressive rendering of the image so far
    #[wasm_bindgen(js_name = imageSoFar)]
    pub fn image_so_far(&self) -> ImageData {
        image_data(self.base, self.len, self.width, self.height)
    }
}

fn image_data(base: usize, len: usize, width: u32, height: u32) -> ImageData {
    // Use the raw access available through `memory.buffer`, but be sure to
    // use `slice` instead of `subarray` to create a copy that isn't backed
    // by `SharedArrayBuffer`. Currently `ImageData` rejects a view of
    // `Uint8ClampedArray` that's backed by a shared buffer.
    //
    // FIXME: that this may or may not be UB based on Rust's rules. For example
    // threads may be doing unsynchronized writes to pixel data as we read it
    // off here. In the context of Wasm this may or may not be UB, we're
    // unclear! In any case for now it seems to work and produces a nifty
    // progressive rendering. A more production-ready application may prefer to
    // instead use some form of signaling here to request an update from the
    // workers instead of synchronously acquiring an update, and that way we
    // could ensure that even on the Rust side of things it's not UB.
    let mem = wasm_bindgen::memory().unchecked_into::<WebAssembly::Memory>();
    let mem = Uint8ClampedArray::new(&mem.buffer()).slice(base as u32, (base + len) as u32);
    ImageData::new_with_js_u8_clamped_array_and_sh(&mem, width, height).unwrap()
}

#[wasm_bindgen(start)]
pub async fn start() {
    console_error_panic_hook::set_once();

    subsecond::wasm_multithreading::init_hotpatch_for_current_thread().await;

    init_hotpatch();

    console::log_1(&"Hello world from Rust WASM!".into());

    let window = web_sys::window().expect("no window");
    let document = window.document().expect("no document");

    let button = document
        .get_element_by_id("testbutton")
        .expect("no testbutton");

    let closure = Closure::<dyn FnMut()>::new(move || {
        subsecond::call(|| {
            console_log!("Test patch data segment");

            // // Not yet working because TLS resets so worker pool resets
            // broadcast_to_workers(|| {
            //     console_log!("Test patch data segment in worker");
            // });
        });

        broadcast_to_workers(|| {
            subsecond::call(|| {
                let thread_id = MY_THREAD_ID.with(|s| *s);

                console_log!("Worker {}: Test patch data segment", thread_id);
            });
        });
    });

    button
        .add_event_listener_with_callback("click", closure.as_ref().unchecked_ref())
        .expect("cannot add listener");

    Reflect::set(&button, &"disabled".into(), &false.into());

    closure.forget();

    // make main thread initialize thread id
    MY_THREAD_ID.with(|s| *s);
}

#[cfg(not(debug_assertions))]
fn init_hotpatch() {
    // empty in release
}

// https://github.com/DioxusLabs/dioxus/blob/main/packages/web/src/devtools.rs
#[cfg(debug_assertions)]
fn init_hotpatch() {
    web_sys::console::info_1(&format!("Initializing hotpatch").into());

    // Get the location of the devserver, using the current location plus the /_dioxus path
    // The idea here being that the devserver is always located on the /_dioxus behind a proxy

    let location = web_sys::window().unwrap().location();
    let url = format!(
        "{protocol}//{host}/_dioxus?build_id={build_id}",
        protocol = match location.protocol().unwrap() {
            prot if prot == "https:" => "wss:",
            _ => "ws:",
        },
        host = location.host().unwrap(),
        build_id = dioxus_cli_config::build_id(),
    );

    let ws = WebSocket::new(&url).unwrap();

    ws.set_onmessage(Some(
        Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            let Ok(text) = e.data().dyn_into::<JsString>() else {
                return;
            };

            // The devserver messages have some &'static strs in them, so we need to leak the source string
            let string: String = text.into();
            let string = Box::leak(string.into_boxed_str());

            match serde_json::from_str::<DevserverMsg>(string) {
                Ok(DevserverMsg::HotReload(hr)) => {
                    if let Some(jumptable) = hr.clone().jump_table {
                        unsafe {
                            subsecond::apply_patch(jumptable);
                        }
                    }
                }

                Ok(DevserverMsg::Shutdown) => {
                    web_sys::console::error_1(&"Connection to the devserver was closed".into())
                }

                Err(e) => web_sys::console::error_1(
                    &format!("Error parsing devserver message: {}", e).into(),
                ),

                Ok(e) => {
                    web_sys::console::info_1(&format!("Ignore devserver message: {:?}", e).into());
                }
            }
        })
        .into_js_value()
        .as_ref()
        .unchecked_ref(),
    ));

    console::log_1(&"Hotpatch initialized".into());
}
