// Silences warnings from the compiler about Work.func and child_entry_point
// being unused when the target is not wasm.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

//! A small module that's intended to provide an example of creating a pool of
//! web workers which can be used to execute `rayon`-style work.

use js_sys::{Object, Reflect};
use std::cell::RefCell;
use std::sync::Arc;
use wasm_bindgen::prelude::*;
use web_sys::{DedicatedWorkerGlobalScope, MessageEvent, WorkerOptions};
use web_sys::{ErrorEvent, Event, Worker};

thread_local! {
    static WORKER_POOL: RefCell<Option<WorkerPool>> = RefCell::new(None);
}

pub struct WorkerPool {
    workers: Vec<Worker>,
}

struct Work {
    func: Box<dyn FnOnce(JsValue) + Send>,
}

impl WorkerPool {
    /// Creates a new `WorkerPool` which immediately creates `initial` workers.
    ///
    /// The pool created here can be used over a long period of time, and it
    /// will be initially primed with `initial` workers. Currently workers are
    /// never released or gc'd until the whole pool is destroyed.
    fn new(initial: usize) -> Result<WorkerPool, JsValue> {
        let mut pool = WorkerPool {
            workers: Vec::with_capacity(initial),
        };
        for _ in 0..initial {
            let worker = pool.spawn()?;
            pool.set_reclaim_callback(worker);
        }

        Ok(pool)
    }

    /// Unconditionally spawns a new worker
    ///
    /// The worker isn't registered with this `WorkerPool` but is capable of
    /// executing work for this Wasm module.
    fn spawn(&self) -> Result<Worker, JsValue> {
        console_log!("spawning new worker");
        // TODO: what to do about `./worker.js`:
        //
        // * the path is only known by the bundler. How can we, as a
        //   library, know what's going on?
        // * How do we not fetch a script N times? It internally then
        //   causes another script to get fetched N times...

        let options: WorkerOptions = WorkerOptions::new();
        options.set_type(web_sys::WorkerType::Module);

        let worker = Worker::new_with_options("./worker.js", &options)?;

        // With a worker spun up send it the module/memory so it can start
        // instantiating the Wasm module. Later it might receive further
        // messages about code to run on the Wasm module.
        let array = js_sys::Array::new();
        array.push(&wasm_bindgen::module());
        array.push(&wasm_bindgen::memory());
        worker.post_message(&array)?;

        Ok(worker)
    }

    /// Fetches a worker from this pool.
    ///
    /// This will attempt to pull an already-spawned web worker from our cache
    /// if one is available, otherwise it will panic.
    fn obtain_worker(&mut self) -> Result<Worker, JsValue> {
        match self.workers.pop() {
            Some(worker) => Ok(worker),
            None => {
                panic!("Currrently creating new worker is not allowed")
            }
        }
    }

    /// Executes the work `f` in a web worker.
    ///
    /// This will acquire a web worker and then send the closure `f` to the
    /// worker to execute. The worker won't be usable for anything else while
    /// `f` is executing. When finished, the worker is automatically reclaimed
    /// back into the pool via the callback set at spawn time.
    ///
    /// # Errors
    ///
    /// Returns any error that may happen while a message is sent to the worker.
    fn execute(
        &mut self,
        f: impl FnOnce(JsValue) + Send + 'static,
        js_payload: JsValue,
    ) -> Result<Worker, JsValue> {
        let worker = self.obtain_worker()?;
        let work = Box::new(Work { func: Box::new(f) });
        let ptr = Box::into_raw(work);

        let to_send = Object::new();
        Reflect::set(&to_send, &"ptr".into(), &JsValue::from(ptr as u32));
        Reflect::set(&to_send, &"jsPayload".into(), &js_payload);

        match worker.post_message(&to_send) {
            Ok(()) => Ok(worker),
            Err(e) => {
                unsafe {
                    drop(Box::from_raw(ptr));
                }
                Err(e)
            }
        }
    }

    /// Configures an `onmessage` callback for the `worker` specified for the
    /// web worker to be reclaimed and re-inserted into this pool when a message
    /// is received.
    ///
    /// Currently this `WorkerPool` abstraction is intended to execute one-off
    /// style work where the work itself doesn't send any notifications and
    /// whatn it's done the worker is ready to execute more work. This method is
    /// used for all spawned workers to ensure that when the work is finished
    /// the worker is reclaimed back into this pool.
    fn set_reclaim_callback(&mut self, worker: Worker) {
        let worker2 = worker.clone();
        let reclaim = Closure::<dyn FnMut(_)>::new(move |event: Event| {
            console_log!("In reclaim callback");
            if let Some(error) = event.dyn_ref::<ErrorEvent>() {
                console_log!("error in worker: {}", error.message());
                return;
            }

            if let Some(msg) = event.dyn_ref::<MessageEvent>() {
                let data = msg.data();
                let msg_type = Reflect::get(&data, &"type".into()).ok();
                match msg_type.as_ref().and_then(|v| v.as_string()).as_deref() {
                    Some("done") => {
                        WORKER_POOL.with(|p| {
                            if let Some(pool) = p.borrow_mut().as_mut() {
                                pool.push_worker(worker2.clone());
                            }
                        });
                    }
                    Some("callback") => {
                        if let Ok(ptr) = Reflect::get(&data, &"ptr".into()) {
                            let ptr = ptr.as_f64().unwrap() as u32;
                            let work = unsafe { Box::from_raw(ptr as *mut Work) };
                            let js_payload = Reflect::get(&data, &"jsPayload".into())
                                .unwrap_or(JsValue::undefined());
                            (work.func)(js_payload);
                        }
                    }
                    _ => {}
                }
            }
        });
        worker.set_onmessage(Some(reclaim.as_ref().unchecked_ref()));
        reclaim.forget(); // Leak intentionally - closure lives as long as worker
        self.workers.push(worker);
    }

    fn push_worker(&mut self, worker: Worker) {
        for prev in self.workers.iter() {
            let prev: &JsValue = prev;
            let worker: &JsValue = &worker;
            assert!(prev != worker);
        }
        self.workers.push(worker);
    }
}

impl WorkerPool {
    /// Executes `f` in a web worker.
    ///
    /// This pool manages a set of web workers to draw from, and `f` will be
    /// spawned quickly into one if the worker is idle. Panics if no idle
    /// workers are available.
    pub fn run(&mut self, f: impl FnOnce() + Send + 'static) -> Result<(), JsValue> {
        self.execute(|_js_payoad| f(), JsValue::undefined())?;
        Ok(())
    }

    pub fn run_with_js_payload(
        &mut self,
        f: impl FnOnce(JsValue) + Send + 'static,
        js_payload: JsValue,
    ) -> Result<(), JsValue> {
        self.execute(f, js_payload)?;
        Ok(())
    }

    pub fn get_worker_count(&self) -> usize {
        self.workers.len()
    }

    /// Returns web worker count.
    pub fn broadcast(
        &mut self,
        f: Arc<dyn Fn(JsValue) + Send + Sync>,
        js_payload: JsValue,
    ) -> Result<(), JsValue> {
        let worker_count = self.workers.len();
        for _ in 0..worker_count {
            let f = f.clone();
            let payload = js_payload.clone();
            self.execute(move |p| f(p), payload)?;
        }
        Ok(())
    }
}

#[wasm_bindgen]
pub fn init_worker_pool(initial: usize) -> Result<(), JsValue> {
    let pool = WorkerPool::new(initial)?;
    WORKER_POOL.with(|p| *p.borrow_mut() = Some(pool));
    Ok(())
}

pub fn with_worker_pool<F, R>(f: F) -> R
where
    F: FnOnce(&mut WorkerPool) -> R
{
    WORKER_POOL.with(|p| {
        let mut pool = p.borrow_mut();
        let pool = pool
            .as_mut()
            .expect("WorkerPool not initialized");
        f(pool)
    })
}

pub fn submit_to_pool(
    f: impl FnOnce() + Send + 'static
) -> Result<(), JsValue> {
    submit_to_pool_with_js_payload(|_| f(), JsValue::undefined())
}

pub fn submit_to_pool_with_js_payload(
    f: impl FnOnce(JsValue) + Send + 'static,
    js_payload: JsValue,
) -> Result<(), JsValue> {
    with_worker_pool(move |pool| -> Result<(), JsValue> {
        pool.execute(f, js_payload)?;
        Ok(())
    })
}

pub fn pool_get_web_worker_num() -> usize {
    with_worker_pool(|pool| pool.get_worker_count())
}

pub fn broadcast_to_workers(
    f: impl Fn() + Send + Sync + 'static
) -> Result<(), JsValue> {
    broadcast_to_workers_with_js(move |_| {
        f()
    }, JsValue::undefined())
}

pub fn broadcast_to_workers_with_js(
    f: impl Fn(JsValue) + Send + Sync + 'static,
    js_payload: JsValue,
) -> Result<(), JsValue> {
    let arcf: Arc<dyn Fn(JsValue) + Send + Sync> = Arc::new(f);

    with_worker_pool(move |pool| {
        pool.broadcast(arcf, js_payload)
    })
}

/// Entry point invoked by `worker.js`
#[wasm_bindgen]
pub fn child_entry_point(ptr: u32, js_payload: JsValue) -> Result<(), JsValue> {
    let ptr = unsafe { Box::from_raw(ptr as *mut Work) };
    let global = js_sys::global().unchecked_into::<DedicatedWorkerGlobalScope>();
    (ptr.func)(js_payload);
    let msg = Object::new();
    Reflect::set(&msg, &"type".into(), &"done".into())?;
    global.post_message(&msg)?;
    Ok(())
}

/// Send a callback from worker to main thread with a JS payload
pub fn worker_send_callback(
    f: Box<dyn FnOnce(JsValue) + Send>,
    js_payload: JsValue,
) -> Result<(), JsValue> {
    let global: DedicatedWorkerGlobalScope = js_sys::global().dyn_into().map_err(|_| {
        JsValue::from_str("worker_send_callback can only be called from a web worker")
    })?;
    let work = Box::new(Work { func: f });
    let ptr = Box::into_raw(work);
    let msg = Object::new();
    Reflect::set(&msg, &"type".into(), &"callback".into())?;
    Reflect::set(&msg, &"ptr".into(), &JsValue::from(ptr as u32))?;
    Reflect::set(&msg, &"jsPayload".into(), &js_payload)?;
    global.post_message(&msg)?;
    Ok(())
}
