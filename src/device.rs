//! This module helps listen for device change notifications by creating a headless window. The
//! headless window is required to use the
//! [`windows_sys::Win32::UI::WindowsAndMessaging::RegisterDeviceNotificationW`] API.
//!
//! This module therefore allows you to listen for device change notifications with out running
//! from the context of a windows service. For example, some services during development will run
//! in as a console application.

use crate::{
    guid,
    hkey::{RegistryData, UnexpectedRegistryData},
    message::DeviceEvent,
    message::{DeviceEventData, DeviceEventType},
    status::StatusHandle,
    wait::{self, Receiver, Sender, WaitResult},
    wchar::{from_wide, to_wide},
};
use crossbeam::queue::SegQueue;
use futures::{ready, Future, Stream};
use parking_lot::Mutex;
use pin_project_lite::pin_project;
use std::{
    borrow::Cow,
    cell::OnceCell,
    collections::HashMap,
    ffi::OsString,
    fmt::{self, Formatter},
    io,
    num::ParseIntError,
    os::windows::io::{AsRawHandle, RawHandle},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Waker},
    thread::JoinHandle,
};
use tracing::{debug, error, trace, warn};
use windows_sys::{
    core::GUID,
    Win32::{Foundation::*, System::LibraryLoader::GetModuleHandleW, UI::WindowsAndMessaging::*},
};

/// Creating Windows requires the hinstance prop of the WinMain function. To retreive this
/// parameter use [`windows_sys::Win32::System::LibraryLoader::GetModuleHandleW`];
fn hinstance() -> isize {
    // Safety: If the handle is NULL, GetModuleHandle returns a handle to the file used to create
    // the calling process
    unsafe { GetModuleHandleW(std::ptr::null()) }
}

/// Window proceedure for responding to windows messages and listening for device notifications
unsafe extern "system" fn device_notification_window_proceedure(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const DeviceNotificationData;
    if !ptr.is_null() {
        match msg {
            // Safety: lparam is a DEV_BROADCAST_HDR when msg is WM_DEVICECHANGE
            WM_DEVICECHANGE => match unsafe { DeviceEvent::try_parse(wparam as _, lparam as _) } {
                Some(msg) => {
                    debug!(?msg.ty);
                    (&*ptr).try_wake_with(Some(msg));
                    0
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            },
            WM_DESTROY => {
                if let Ok(window) = crate::get_window_text!(hwnd, 128) {
                    trace!(?window, "wm_destroy");
                }
                let arc = Arc::from_raw(ptr as *const DeviceNotificationData);
                arc.try_wake_with(None);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    } else {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

/// Create an instance of a DeviceNotifier window.
///
/// Safety: name must be a null terminated Wide string, and user_data must be a pointer to an
/// Arc<DeviceNotificationData>;
unsafe fn create_device_notification_window(
    name: *const u16,
    user_data: isize,
) -> io::Result<RecepientHandle> {
    let handle = CreateWindowExW(
        WS_EX_APPWINDOW,   // styleEx
        WINDOW_CLASS_NAME, // class name
        name,              // window name
        WS_MINIMIZE,       // style
        0,                 // x
        0,                 // y
        CW_USEDEFAULT,     // width
        CW_USEDEFAULT,     // hight
        0,                 // parent
        0,                 // menu
        hinstance(),       // instance
        std::ptr::null(),  // data
    );
    match handle {
        0 => Err(io::Error::last_os_error()),
        handle => {
            // NOTE a 0 is returned if their is a failure, or if the previous pointer was NULL. To
            // distinguish if a true error has occured we have to clear any errors and test the
            // last_os_error == 0 or not.
            let prev = unsafe {
                SetLastError(0);
                SetWindowLongPtrW(handle, GWLP_USERDATA, user_data)
            };
            match prev {
                0 => match unsafe { GetLastError() } as _ {
                    0 => Ok(Window(handle).into()),
                    raw => Err(io::Error::from_raw_os_error(raw)),
                },
                _ => Ok(Window(handle).into()),
            }
        }
    }
}

/// Dispatch window messages
///
/// We receive a "name", a list of GUID registrations, and some "user_data" which is an arc.
///
/// Safety: user_data must be a pointer to an Arc<DeviceNotificationData> that was created
/// by Arc::into_raw...
///
/// This method will rebuild the Arc and pass it to the window procedure...
unsafe fn device_notification_window_dispatcher(
    name: OsString,
    registrations: NotificationRegistry,
    user_data: isize,
) -> io::Result<()> {
    // TODO figure out how to pass atom into class name
    let _atom = get_window_class();
    let unsafe_name = to_wide(name.clone());
    let arc = Arc::from_raw(user_data as *const Arc<DeviceNotificationData>);
    trace!(?name, "starting window dispatcher");
    let hwnd = create_device_notification_window(unsafe_name.as_ptr(), Arc::as_ptr(&arc) as _)?;
    // Register the device notifications
    let _registry = registrations.register(&hwnd, hwnd.discriminant())?;

    let mut msg: MSG = std::mem::zeroed();
    loop {
        match GetMessageW(&mut msg as *mut _, 0, 0, 0) {
            0 => {
                trace!(?name, "window dispatcher finished");
                break Ok(());
            }
            -1 => {
                let error = Err(io::Error::last_os_error());
                error!(?name, ?error, "window dispatcher error");
                break error;
            }
            _ if msg.message == WM_CLOSE as _ => {
                trace!(?name, "window dispatcher received wm_close");
                TranslateMessage(&msg as *const _);
                DispatchMessageW(&msg as *const _);
                break Ok(());
            }
            _ => {
                TranslateMessage(&msg as *const _);
                DispatchMessageW(&msg as *const _);
            }
        }
    }
}

/// The name of our window class.
/// [See also](https://learn.microsoft.com/en-us/windows/win32/winmsg/about-window-classes)
const WINDOW_CLASS_NAME: *const u16 = windows_sys::w!("DeviceNotifier");

/// We register our class only once
const WINDOW_CLASS_ATOM: OnceCell<u16> = OnceCell::new();
fn get_window_class() -> u16 {
    *WINDOW_CLASS_ATOM.get_or_init(|| {
        let class = WNDCLASSEXW {
            style: 0,
            hIcon: 0,
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as _,
            hIconSm: 0,
            hCursor: 0,
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: WINDOW_CLASS_NAME,
            lpfnWndProc: Some(device_notification_window_proceedure),
            hbrBackground: 0,
        };
        match unsafe { RegisterClassExW(&class as *const _) } {
            0 => panic!("{:?}", io::Error::last_os_error()),
            atom => atom,
        }
    })
}

/// Scan the USB device registry.
///
/// This routine will perform 2 registry lookups. First scan
/// `SYSTEM\\CurrentControlSet\\Control\\Com Name Arbiter\\Devices` to get a mapping from COM ports
/// to Vendor/Product ID's.
///
/// Then will scan HARDWARE\\DEVICEMAP\\SERIALCOMM registry to get a list of currently connected
/// devices.  Then we have all the information to provide a hashmap of currently connected USB COM
/// ports including the Vendor/Product ID's.
pub fn scan() -> Result<HashMap<OsString, UsbVidPid>, ScanError> {
    // We collect all the currently connected COM ports from the registry
    let connected = crate::util::hkey::open(
        crate::util::hkey::PredefinedHkey::LOCAL_MACHINE,
        "HARDWARE\\DEVICEMAP\\SERIALCOMM",
    )?
    .into_values()?
    .map(|value| value?.1.try_into_os_string().map_err(ScanError::from))
    .collect::<Result<Vec<OsString>, ScanError>>()?;

    // We collect all the vender and product id's from the registry
    let devices = crate::util::hkey::open(
        crate::util::hkey::PredefinedHkey::LOCAL_MACHINE,
        "SYSTEM\\CurrentControlSet\\Control\\COM Name Arbiter\\Devices",
    )?
    .into_values()?
    .map(|value| {
        let (port, data) = value?;
        UsbVidPid::try_from(data).map(|vidpid| (port, vidpid))
    })
    .collect::<Result<HashMap<OsString, UsbVidPid>, ScanError>>()?;

    // Filter the registry map to only list connected devices We loop again because we want to
    // properly capture errors
    Ok(devices
        .into_iter()
        .filter(|(port, _)| connected.contains(&port))
        .collect())
}

/// Scan all the connected usb devices, and return the ID's for a chosen port (if it exists)
pub fn scan_for(port: &OsString) -> Result<UsbVidPid, ScanError> {
    trace!(?port, "scanning for usb device");
    self::scan()
        .map(|mut devices| devices.remove(port))?
        .ok_or_else(|| ScanError::ComPortMissingFromRegistry(port.to_owned()))
}

#[derive(thiserror::Error, Debug)]
pub enum ScanError {
    #[error("unexpected registry data => {0}")]
    UnexpectedRegistryData(#[from] UnexpectedRegistryData),
    #[error("io error => {0}")]
    Io(#[from] io::Error),
    #[error("invalid registry data {0:?} {1:?}")]
    InvalidRegistryData(ParseIntError, OsString),
    #[error("com port {0:?} missing from registry")]
    ComPortMissingFromRegistry(OsString),
}

#[derive(Copy, Clone, PartialEq)]
pub struct UsbVidPid {
    vid: u32,
    pid: u32,
}

impl UsbVidPid {
    pub fn vid(&self) -> String {
        format!("{:0>4X}", self.vid)
    }

    pub fn pid(&self) -> String {
        format!("{:0>4X}", self.pid)
    }

    pub fn matches(&self, vid: &str, pid: &str) -> bool {
        vid == self.vid() && pid == self.pid()
    }
}

impl fmt::Debug for UsbVidPid {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("UsbRegistryDevice")
            .field("vid", &self.vid())
            .field("pid", &self.pid())
            .finish()
    }
}

impl TryFrom<RegistryData> for UsbVidPid {
    type Error = ScanError;
    fn try_from(value: RegistryData) -> Result<Self, Self::Error> {
        let os_str = value.try_into_os_string()?;
        let data = os_str
            .to_str()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "unsupported registry value"))?;
        Self::try_from((&data[12..16], &data[21..25]))
            .map_err(|e| ScanError::InvalidRegistryData(e, os_str))
    }
}

impl<'v, 'p, V, P> TryFrom<(V, P)> for UsbVidPid
where
    V: Into<Cow<'v, str>>,
    P: Into<Cow<'p, str>>,
{
    type Error = ParseIntError;
    fn try_from((vid, pid): (V, P)) -> Result<Self, Self::Error> {
        let vid = u32::from_str_radix(&vid.into(), 16)?;
        let pid = u32::from_str_radix(&pid.into(), 16)?;
        Ok(Self { vid, pid })
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for UsbVidPid {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("UsbVidPid", 2)?;
        state.serialize_field("vid", &self.vid())?;
        state.serialize_field("pid", &self.pid())?;
        state.end()
    }
}

/// A RAII guard for a window which will destroy the window when dropped
pub struct Window(HWND);
impl Drop for Window {
    fn drop(&mut self) {
        let _ = unsafe { DestroyWindow(self.0) };
    }
}
impl AsRawHandle for Window {
    fn as_raw_handle(&self) -> RawHandle {
        self.0 as _
    }
}

/// Device notification handles returned by
/// [`windows_sys::Win32::UI::WindowsAndMessaging::RegisterDeviceNotificationW`] must be closed by
/// calling the [`windows_sys::Win32::UI::WindowsAndMessaging::UnregisterDeviceNotification`]
/// function when they are no longer needed.
///
/// This struct is a RAII guard to ensure notification handles are properly closed
pub struct RegistrationHandle(HDEVNOTIFY);
impl Drop for RegistrationHandle {
    fn drop(&mut self) {
        let _ = unsafe { UnregisterDeviceNotification(self.0) };
    }
}

/// Register device notifications for either a "window" or a "service". See the Flags parameter in:
/// [https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-registerdevicenotificationw]
#[repr(u32)]
pub enum RecepientHandle {
    /// The message recipient parameter is a window handle
    Window(Window) = DEVICE_NOTIFY_WINDOW_HANDLE,
    /// The message recipient parameter is a service handle
    Service(isize) = DEVICE_NOTIFY_SERVICE_HANDLE,
}
impl RecepientHandle {
    fn discriminant(&self) -> u32 {
        // safety: https://doc.rust-lang.org/reference/items/enumerations.html#pointer-casting
        // If the enumeration specifies a primitive representation, then the discriminant may
        // be reliably accessed via unsafe pointer casting:
        unsafe { *(self as *const Self as *const u32) }
    }
}
impl AsRawHandle for RecepientHandle {
    fn as_raw_handle(&self) -> RawHandle {
        match self {
            Self::Window(handle) => handle.as_raw_handle(),
            Self::Service(handle) => *handle as _,
        }
    }
}

impl From<Window> for RecepientHandle {
    fn from(value: Window) -> Self {
        RecepientHandle::Window(value)
    }
}

/// Register to receive device notifications for DBT_DEVTYP_DEVICE_INTERFACE or DBT_DEVTYP_HANDLE.
/// We wrap this registration process. To extend support for other kinds of devices, see:
/// https://learn.microsoft.com/en-us/windows-hardware/drivers/install/system-defined-device-setup-classes-available-to-vendors?redirectedfrom=MSDN
pub struct NotificationRegistry(Vec<GUID>);
impl NotificationRegistry {
    /// Windows CE USB ActiveSync Devices
    pub const WCEUSBS: GUID =
        guid!(0x25dbce51, 0x6c8f, 0x4a72, 0x8a, 0x6d, 0xb5, 0x4c, 0x2b, 0x4f, 0xc8, 0x35);
    pub const USBDEVICE: GUID =
        guid!(0x88BAE032, 0x5A81, 0x49f0, 0xBC, 0x3D, 0xA4, 0xFF, 0x13, 0x82, 0x16, 0xD6);
    pub const PORTS: GUID =
        guid!(0x4d36e978, 0xe325, 0x11ce, 0xbf, 0xc1, 0x08, 0x00, 0x2b, 0xe1, 0x03, 0x18);

    /// Create a new registry
    pub fn new() -> Self {
        Self::with_capacity(4)
    }

    /// Create a new registry with fixed capacity
    pub fn with_capacity(capacity: usize) -> Self {
        Self(Vec::with_capacity(capacity))
    }

    /// Helper to add all USB serial port notifications
    pub fn with_serial_port(self) -> Self {
        self.with(NotificationRegistry::WCEUSBS)
            .with(NotificationRegistry::USBDEVICE)
            .with(NotificationRegistry::PORTS)
    }

    /// Add a GUID to the registration
    pub fn with(mut self, guid: GUID) -> Self {
        self.0.push(guid);
        self
    }

    pub fn spawn<N>(self, n: N) -> Result<DeviceNotificationListener, ScanError>
    where
        N: Into<OsString> + Send + Sync + 'static,
    {
        let name: OsString = n.into();
        let window = name.clone();
        let ours = Arc::new(DeviceNotificationData::new()?);
        let theirs = Arc::clone(&ours);
        let join_handle = std::thread::spawn(move || unsafe {
            device_notification_window_dispatcher(name, self, Arc::into_raw(theirs) as _)
        });
        Ok(DeviceNotificationListener {
            window,
            context: ours,
            join_handle: Some(join_handle),
        })
    }

    /// Collect the GUID's and register them for a window handle. NOTE that this method is private
    /// and not called directly.  The registration is expected to be passed to another thread which
    /// starts the listener
    fn register<H: AsRawHandle>(self, raw: &H, kind: u32) -> io::Result<Vec<RegistrationHandle>> {
        // Safety: We initialize the DEV_BROADCAST_DEVICEINTERFACE_W header correctly before use.
        self.0
            .into_iter()
            .map(|guid| {
                let handle = unsafe {
                    let mut iface = std::mem::zeroed::<DEV_BROADCAST_DEVICEINTERFACE_W>();
                    iface.dbcc_size = std::mem::size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as _;
                    iface.dbcc_classguid = guid;
                    iface.dbcc_devicetype = DBT_DEVTYP_DEVICEINTERFACE;
                    RegisterDeviceNotificationW(
                        raw.as_raw_handle() as _,
                        &iface as *const _ as _,
                        kind,
                    )
                };
                match handle.is_null() {
                    false => Ok(RegistrationHandle(handle)),
                    true => Err(io::Error::last_os_error()),
                }
            })
            .collect::<io::Result<Vec<RegistrationHandle>>>()
    }
}

struct DeviceNotificationData {
    queue: SegQueue<Option<DeviceEvent>>,
    waker: Mutex<Option<Waker>>,
}

impl DeviceNotificationData {
    fn new() -> Result<Self, ScanError> {
        let queue = SegQueue::new();
        let devices = self::scan()?;
        for (port, _vidpid) in devices.into_iter() {
            debug!(?port, "found existing USB device");
            queue.push(Some(DeviceEvent {
                ty: DeviceEventType::Arrival,
                data: DeviceEventData::Port(port),
            }));
        }
        Ok(Self {
            queue,
            waker: Mutex::new(None),
        })
    }

    fn try_wake(&self) -> &Self {
        if let Some(waker) = &self.waker.lock().as_ref() {
            waker.wake_by_ref()
        }
        self
    }

    fn try_wake_with(&self, ev: Option<DeviceEvent>) -> &Self {
        self.queue.push(ev);
        self.try_wake();
        self
    }

    fn register(&self, context: &Context<'_>) {
        let new_waker = context.waker();
        let mut waker = self.waker.lock();
        *waker = match waker.take() {
            None => Some(new_waker.clone()),
            Some(old_waker) => {
                if old_waker.will_wake(new_waker) {
                    Some(old_waker)
                } else {
                    Some(new_waker.clone())
                }
            }
        }
    }
}

/// A stream of device notifications
pub struct DeviceNotificationListener {
    /// The name of the window on the remote thread
    window: OsString,
    /// Registered notifications, stored here to prevent drops
    /// Shared state with window procedure to create a stream of device notifications
    context: Arc<DeviceNotificationData>,
    /// Handle to a window dispatcher
    join_handle: Option<JoinHandle<io::Result<()>>>,
}

impl DeviceNotificationListener {
    pub fn listen(&self) -> DeviceNotificationStream {
        DeviceNotificationStream(Arc::clone(&self.context))
    }

    pub fn scan(&self) -> Result<&Self, ScanError> {
        let devices = self::scan()?;
        for (port, _) in devices.into_iter() {
            debug!(?port, "found USB device");
            self.context.queue.push(Some(DeviceEvent {
                ty: DeviceEventType::Arrival,
                data: DeviceEventData::Port(port),
            }));
        }
        Ok(self)
    }

    pub fn close(&mut self) -> io::Result<()> {
        // Find the window so we can close it
        trace!(window = ?self.window, "closing device notification listener");
        let wide = to_wide(self.window.clone());
        let hwnd = unsafe {
            let result = FindWindowW(WINDOW_CLASS_NAME, wide.as_ptr());
            match result {
                0 => Err(io::Error::last_os_error()),
                hwnd => Ok(hwnd),
            }
        }?;

        // Close the window
        let _close = unsafe {
            let result = PostMessageW(hwnd, WM_CLOSE, 0, 0);
            match result {
                0 => Err(io::Error::last_os_error()),
                _ => Ok(()),
            }
        }?;
        let jh = self.join_handle.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "Already closed DeviceNotificationListener",
            )
        })?;
        jh.join()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "join error"))?
    }
}

impl Drop for DeviceNotificationListener {
    fn drop(&mut self) {
        match self.close() {
            Ok(_) => trace!(window=?self.window, "DeviceNotificationListener drop OK"),
            Err(error) => {
                trace!(window=?self.window, ?error, "DeviceNotificationListener drop error")
            }
        }
    }
}

pub struct DeviceNotificationStream(Arc<DeviceNotificationData>);

impl Stream for DeviceNotificationStream {
    type Item = DeviceEvent;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.register(cx);
        debug!(len = self.0.queue.len(), "DeviceNotificationListener poll");

        match self.0.queue.pop() {
            None => Poll::Pending,
            Some(Some(inner)) => {
                debug!(ev=?inner.ty, "usb event");
                Poll::Ready(Some(inner))
            }
            Some(None) => {
                debug!("DeviceNotificationListener stream end");
                Poll::Ready(None)
            }
        }
    }
}

#[derive(Debug)]
pub enum PlugEvent {
    Plug(OsString),
    Unplug(OsString),
}

pub fn plug_events(ev: DeviceEvent) -> Option<PlugEvent> {
    match ev {
        DeviceEvent {
            ty: DeviceEventType::Arrival,
            data: DeviceEventData::Port(port),
        } => Some(PlugEvent::Plug(port)),
        DeviceEvent {
            ty: DeviceEventType::RemoveComplete,
            data: DeviceEventData::Port(port),
        } => Some(PlugEvent::Unplug(port)),
        _ => None,
    }
}

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
    pub ids: UsbVidPid,
    /// A future which resolves when the COM port is unplugged
    pub unplugged: Unplugged,
}

impl TrackedPort {
    pub fn track(port: OsString, ids: UsbVidPid) -> io::Result<(Sender, TrackedPort)> {
        let (sender, receiver) = wait::oneshot()?;
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
    Scan(#[from] ScanError),
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
            ids: Vec<UsbVidPid>,
            cache: HashMap<OsString, Sender>
        },
        Complete
    }
}

impl<St> Stream for Tracking<St>
where
    St: Stream<Item = PlugEvent>,
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
                    Poll::Ready(Some(PlugEvent::Plug(port))) => match scan_for(&port) {
                        Err(e) => break Poll::Ready(Some(Err(e.into()))),
                        Ok(id) => match ids.iter().find(|test| **test == id) {
                            None => debug!(?port, ?id, "ignoring com device"),
                            Some(id) => match TrackedPort::track(port.clone(), *id) {
                                Err(e) => break Poll::Ready(Some(Err(e.into()))),
                                Ok((sender, tracked)) => {
                                    cache.insert(port.clone(), sender);
                                    break Poll::Ready(Some(Ok(tracked)));
                                }
                            },
                        },
                    },
                    Poll::Ready(Some(PlugEvent::Unplug(port))) => match cache.remove(&port) {
                        None => warn!(?port, "untracked port"),
                        Some(ids) => match ids.set() {
                            Ok(_) => debug!(?port, "unplugged signal sent"),
                            Err(e) => break Poll::Ready(Some(Err(e.into()))),
                        },
                    },
                },
                TrackingProj::Complete => {
                    panic!("Watch must not be polled after stream has finished")
                }
            }
        }
    }
}

pub trait DeviceStreamExt: Stream<Item = PlugEvent> {
    fn track<'v, 'p, V, P>(self, ids: Vec<(V, P)>) -> Result<Tracking<Self>, ParseIntError>
    where
        V: Into<Cow<'v, str>>,
        P: Into<Cow<'p, str>>,
        Self: Sized,
    {
        let collection = ids
            .into_iter()
            .map(UsbVidPid::try_from)
            .collect::<Result<Vec<UsbVidPid>, ParseIntError>>()?;
        Ok(Tracking::Streaming {
            inner: self,
            ids: collection,
            cache: HashMap::new(),
        })
    }
}

impl<T: ?Sized> DeviceStreamExt for T where T: Stream<Item = PlugEvent> {}

pub mod prelude {
    pub use super::DeviceStreamExt;
}
