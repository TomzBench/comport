//! comport

#[cfg(test)]
mod tests;

// TODO remove pub when we add async io to com port
pub mod channel;
pub mod event;
mod guid;
mod hkey;
mod wchar;
mod wm;

pub use hkey::{PortMeta, RegistryError};
use std::{collections::HashMap, ffi::OsString, io};
pub use wm::{PlugEvent, WindowEvents};

/// Listen for [`wm::WindowEvents`]
pub fn listen<N>(name: N) -> Result<wm::WindowEvents, hkey::RegistryError>
where
    N: Into<OsString> + Send + Sync + 'static,
{
    wm::Registry::new().with_serial_port().spawn(name)
}

/// Get a hash map of all the currently connected devices
pub fn scan() -> hkey::ScanResult<HashMap<OsString, hkey::PortMeta>> {
    hkey::scan()
}

/// If you have a previous call to [`listen`], than you can have the listener stream re-emit
/// currently connected devices
pub fn rescan<N>(name: N) -> io::Result<()>
where
    N: Into<OsString>,
{
    wm::rescan(name)
}

pub mod prelude {
    use crate::{
        event::{Receiver, Sender, WaitResult},
        hkey::{PortMeta, RegistryError, ScanResult},
        wm::PlugEvent,
    };
    use futures::{ready, Future, Stream};
    use pin_project_lite::pin_project;
    use std::{
        borrow::Cow,
        collections::HashMap,
        ffi::OsString,
        io,
        num::ParseIntError,
        pin::Pin,
        task::{Context, Poll},
    };
    use tracing::{debug, warn};

    pin_project! {
        #[project = UnpluggedProj]
        #[project_replace = UnpluggedProjReplace]
        #[derive(Debug)]
        #[must_use = "futures do nothing unless you `.await` or poll them"]
        pub enum Unplugged {
            Waiting {
                #[pin]
                inner: Receiver,
            },
            Complete
        }
    }

    impl Future for Unplugged {
        type Output = WaitResult;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            match self.as_mut().project() {
                UnpluggedProj::Waiting { inner } => {
                    let result = ready!(inner.poll(cx));
                    self.project_replace(Unplugged::Complete);
                    Poll::Ready(result)
                }
                UnpluggedProj::Complete => panic!("Unplugged cannot be polled after complete"),
            }
        }
    }

    /// A tracked port emitted from the [`DeviceStreamExt::track`]
    #[derive(Debug)]
    pub struct TrackedPort {
        /// The com port name. IE: COM4
        pub port: OsString,
        /// The Vendor/Product ID's of the serial port
        pub ids: PortMeta,
        /// A future which resolves when the COM port is unplugged
        pub unplugged: Unplugged,
    }

    impl TrackedPort {
        pub fn track(port: OsString, ids: PortMeta) -> io::Result<(Sender, TrackedPort)> {
            let (sender, receiver) = crate::event::oneshot()?;
            let port = TrackedPort {
                port,
                ids,
                unplugged: Unplugged::Waiting { inner: receiver },
            };
            Ok((sender, port))
        }
    }

    #[derive(thiserror::Error, Debug)]
    pub enum TrackingError {
        #[error("io error => {0}")]
        Io(#[from] io::Error),
        #[error("scan error => {0}")]
        Scan(#[from] RegistryError),
    }

    pin_project! {
        #[project = TrackingProj]
        #[project_replace = TrackingProjReplace]
        #[derive(Debug)]
        #[must_use = "futures do nothing unless you `.await` or poll them"]
        pub enum Tracking<St> {
            Streaming {
                #[pin]
                inner: St,
                ids: Vec<PortMeta>,
                cache: HashMap<OsString, Sender>
            },
            Complete
        }
    }

    impl<St> Stream for Tracking<St>
    where
        St: Stream<Item = ScanResult<PlugEvent>>,
    {
        type Item = Result<TrackedPort, TrackingError>;
        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            loop {
                match self.as_mut().project() {
                    TrackingProj::Streaming { inner, ids, cache } => match inner.poll_next(cx) {
                        Poll::Pending => break Poll::Pending,
                        Poll::Ready(None) => {
                            self.project_replace(Self::Complete);
                            break Poll::Ready(None);
                        }
                        Poll::Ready(Some(Err(e))) => break Poll::Ready(Some(Err(e.into()))),
                        Poll::Ready(Some(Ok(PlugEvent::Arrival(port, id)))) => {
                            match ids.iter().find(|test| **test == id) {
                                None => debug!(?port, ?id, "ignoring com device"),
                                Some(id) => match TrackedPort::track(port.clone(), id.clone()) {
                                    Err(e) => break Poll::Ready(Some(Err(e.into()))),
                                    Ok((sender, tracked)) => {
                                        cache.insert(port.clone(), sender);
                                        break Poll::Ready(Some(Ok(tracked)));
                                    }
                                },
                            }
                        }
                        Poll::Ready(Some(Ok(PlugEvent::RemoveComplete(port)))) => {
                            match cache.remove(&port) {
                                None => warn!(?port, "untracked port"),
                                Some(ids) => match ids.set() {
                                    Ok(_) => debug!(?port, "unplugged signal sent"),
                                    Err(e) => break Poll::Ready(Some(Err(e.into()))),
                                },
                            }
                        }
                    },
                    TrackingProj::Complete => {
                        panic!("Watch must not be polled after stream has finished")
                    }
                }
            }
        }
    }

    pub trait DeviceStreamExt: Stream<Item = ScanResult<PlugEvent>> {
        fn track<'v, 'p, V, P>(self, ids: Vec<(V, P)>) -> Result<Tracking<Self>, ParseIntError>
        where
            V: Into<Cow<'v, str>>,
            P: Into<Cow<'p, str>>,
            Self: Sized,
        {
            let collection = ids.into_iter().map(PortMeta::from).collect();
            Ok(Tracking::Streaming {
                inner: self,
                ids: collection,
                cache: HashMap::new(),
            })
        }
    }

    impl<T: ?Sized> DeviceStreamExt for T where T: Stream<Item = ScanResult<PlugEvent>> {}
}
