//! Node binding

use crate::{hkey::PortMeta, prelude::*, wm::WindowEvents};
use futures::Stream;
use neon::prelude::*;

impl Finalize for WindowEvents {
    fn finalize<'a, C: Context<'a>>(mut self, _: &mut C) {
        if let Err(e) = self.close() {
            panic!("failed to close WindowEvents {e}");
        }
    }
}

impl PortMeta {
    fn to_neon_obj<'cx, C>(&self, cx: &mut C) -> JsResult<'cx, JsObject>
    where
        C: Context<'cx>,
    {
        let entry = cx.empty_object();
        let vendor = cx.string(self.vid());
        let product = cx.string(self.pid());
        entry.set(cx, "vendor", vendor)?;
        entry.set(cx, "product", product)?;
        Ok(entry)
    }
}

fn scan(mut cx: FunctionContext) -> JsResult<JsObject> {
    let map = match crate::scan() {
        Ok(value) => Ok(value),
        Err(e) => {
            let error = cx.error(e.to_string())?;
            cx.throw(error)
        }
    }?;
    let ret = cx.empty_object();
    for (port, meta) in map
        .into_iter()
        .filter_map(|(port, meta)| port.to_str().map(|s| (s.to_string(), meta)))
    {
        let obj = meta.to_neon_obj(&mut cx)?;
        ret.set(&mut cx, port.as_str(), obj)?;
    }
    Ok(ret)
}

/// TODO read the Name prop and call [`crate::rescan`]
fn rescan(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    Ok(cx.undefined())
}

/// TODO - except a callback and spawn a runtime to drive the listen
fn listen(mut cx: FunctionContext) -> JsResult<JsBox<WindowEvents>> {
    let name = cx.argument::<JsString>(0)?.value(&mut cx);
    let listen = match crate::listen(name) {
        Ok(value) => Ok(value),
        Err(e) => {
            let error = cx.error(e.to_string())?;
            cx.throw(error)
        }
    }?;
    Ok(cx.boxed(listen))
}

/// TODO - we can't pass generics across ffi boundary so we do not chain the calls to a stream like
/// in the rust api. Instead we create a new method for each type of stream the caller is
/// interested in. In this type of event stream the caller is interested in tracking vendor and
/// product ID's.
fn track(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    Ok(cx.undefined())
}

#[neon::main]
fn main(mut cx: ModuleContext) -> NeonResult<()> {
    cx.export_function("scan", scan)?;
    cx.export_function("rescan", rescan)?;
    cx.export_function("listen", listen)?;
    cx.export_function("track", track)?;
    Ok(())
}
