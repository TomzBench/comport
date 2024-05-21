#![deny(clippy::all)]

#[macro_use]
extern crate napi_derive;
use napi::{Error, Result};
use std::collections::HashMap;

#[napi(object)]
pub struct Port {
  pub vendor: String,
  pub product: String,
}

impl From<comport::PortMeta> for Port {
  fn from(value: comport::PortMeta) -> Self {
    Port {
      vendor: value.vid(),
      product: value.pid(),
    }
  }
}

#[napi]
pub fn scan() -> Result<HashMap<String, Port>> {
  let map = comport::scan()
    .map_err(|e| Error::from_reason(e.to_string()))?
    .into_iter()
    .filter_map(|(port, meta)| port.to_str().map(|s| (s.to_string(), Port::from(meta))))
    .collect();
  Ok(map)
}

#[napi]
pub fn rescan(name: String) -> Result<()> {
  comport::rescan(name).map_err(|e| Error::from_reason(e.to_string()))
}
