#![deny(clippy::all)]

#[macro_use]
extern crate napi_derive;
use comport::{
    event::{Receiver as Abort, Sender as AbortSet},
    prelude::*,
};
use futures::{future::Shared, FutureExt, StreamExt};
use napi::{
    bindgen_prelude::ObjectFinalize,
    threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode},
    Error, JsFunction, Result,
};
use serde::Serialize;
use std::{collections::HashMap, pin::pin, thread::JoinHandle};

#[napi]
pub struct TrackedPort {
    pub port: String,
    pub meta: PortMeta,
    unplugged: Shared<Unplugged>,
}

#[napi]
impl TrackedPort {
    #[napi]
    pub async fn unplugged(&self) -> Result<()> {
        self.unplugged
            .clone()
            .await
            .map_err(|e| Error::from_reason(e.to_string()))
    }
}

impl From<comport::prelude::TrackedPort> for TrackedPort {
    fn from(value: comport::prelude::TrackedPort) -> Self {
        TrackedPort {
            port: value.port.to_str().unwrap_or("unknown").to_string(),
            meta: value.ids.into(),
            unplugged: value.unplugged.shared(),
        }
    }
}

#[derive(Serialize, Debug)]
#[serde(tag = "type")]
pub enum PlugEvent {
    Plug { port: String, meta: PortMeta },
    Unplug { port: String },
}

impl From<comport::PlugEvent> for PlugEvent {
    fn from(value: comport::PlugEvent) -> Self {
        match value {
            comport::PlugEvent::Arrival(port, meta) => PlugEvent::Plug {
                port: port.to_str().unwrap_or("unknown").to_string(),
                meta: meta.into(),
            },
            comport::PlugEvent::RemoveComplete(port) => PlugEvent::Unplug {
                port: port.to_str().unwrap_or("unknown").to_string(),
            },
        }
    }
}

#[napi(object)]
#[derive(Clone, Debug, Serialize)]
pub struct PortMeta {
    pub vendor: String,
    pub product: String,
}

impl From<comport::PortMeta> for PortMeta {
    fn from(value: comport::PortMeta) -> Self {
        PortMeta {
            vendor: value.vid(),
            product: value.pid(),
        }
    }
}

#[napi(custom_finalize)]
pub struct AbortHandle {
    abort: Option<AbortSet>,
    join_handle: Option<JoinHandle<()>>,
}

#[napi]
impl AbortHandle {
    #[napi]
    pub fn abort(&mut self) -> Result<()> {
        match self.abort.take() {
            None => Ok(()),
            Some(abort) => {
                abort.set().map_err(|e| Error::from_reason(e.to_string()))?;
                if let Some(jh) = self.join_handle.take() {
                    let _result = jh.join();
                }
                Ok(())
            }
        }
    }
}

impl ObjectFinalize for AbortHandle {
    fn finalize(mut self, _env: napi::Env) -> Result<()> {
        self.abort()
    }
}

fn abort_channel() -> Result<(AbortSet, Abort)> {
    comport::event::oneshot().map_err(|e| Error::from_reason(e.to_string()))
}

#[napi]
pub fn scan() -> Result<HashMap<String, PortMeta>> {
    let map = comport::scan()
        .map_err(|e| Error::from_reason(e.to_string()))?
        .into_iter()
        .filter_map(|(port, meta)| port.to_str().map(|s| (s.to_string(), PortMeta::from(meta))))
        .collect();
    Ok(map)
}

#[napi]
pub fn rescan(name: String) -> Result<()> {
    comport::rescan(name).map_err(|e| Error::from_reason(e.to_string()))
}

#[napi(ts_args_type = "name: string, callback: (err:null | Error, event: any) => void")]
pub fn listen(name: String, callback: JsFunction) -> Result<AbortHandle> {
    // Create a callback to emit events into javascript land
    let tsfn: ThreadsafeFunction<PlugEvent> = callback.create_threadsafe_function(0, |cx| {
        serde_json::to_value(cx.value)
            .map(|result| vec![result])
            .map_err(|e| Error::from_reason(e.to_string()))
    })?;

    // Get an abort handle to return to the caller
    let (abort_set, abort) = abort_channel()?;

    // Create an event stream
    let stream = comport::listen(name)
        .map_err(|e| Error::from_reason(e.to_string()))?
        .take_until(abort);

    // Spawn a thread to listen for events
    let jh = std::thread::spawn(move || {
        futures::executor::block_on(async {
            let mut pinned = pin!(stream);
            while let Some(ev) = pinned.next().await {
                let _status = match ev {
                    Ok(ev) => tsfn.call(
                        Ok(PlugEvent::from(ev)),
                        ThreadsafeFunctionCallMode::Blocking,
                    ),
                    Err(e) => tsfn.call(
                        Err(Error::from_reason(e.to_string())),
                        ThreadsafeFunctionCallMode::Blocking,
                    ),
                };
            }
        });
    });
    Ok(AbortHandle {
        join_handle: Some(jh),
        abort: Some(abort_set),
    })
}

///      - Copy listen() implementation but except a Vec<(String,String)> of Product/Vendor ids and
///        emit a Track event which includes a Unplug promise
#[napi]
pub fn track(
    name: String,
    ids: Vec<(String, String)>,
    #[napi(ts_arg_type = "(err: null | Error, event: any) => void")] callback: JsFunction,
) -> Result<AbortHandle> {
    // Create a callback to emit events into javascript land
    let tsfn: ThreadsafeFunction<TrackedPort> =
        callback.create_threadsafe_function(0, |cx| Ok(vec![cx.value]))?;

    // Get an abort handle to return to the caller
    let (abort_set, abort) = abort_channel()?;

    // Create an event stream
    let stream = comport::listen(name)
        .map_err(|e| Error::from_reason(e.to_string()))?
        .take_until(abort)
        .track(ids)
        .map_err(|e| Error::from_reason(e.to_string()))?;

    // Spawn a thread to listen for events
    let jh = std::thread::spawn(move || {
        futures::executor::block_on(async {
            let mut pinned = pin!(stream);
            while let Some(ev) = pinned.next().await {
                let _status = match ev {
                    Ok(ev) => tsfn.call(
                        Ok(TrackedPort::from(ev)),
                        ThreadsafeFunctionCallMode::Blocking,
                    ),
                    Err(e) => tsfn.call(
                        Err(Error::from_reason(e.to_string())),
                        ThreadsafeFunctionCallMode::Blocking,
                    ),
                };
            }
        });
    });
    Ok(AbortHandle {
        join_handle: Some(jh),
        abort: Some(abort_set),
    })
}
