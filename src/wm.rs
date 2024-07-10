//! Windows Messaging
//!
//! This module uses a windowless window to receive Windows Messages from which to receive device
//! notifications

use crate::{
    guid,
    hkey::{self, scan, PortMeta, ScanResult},
    wchar::{self, from_wide, to_wide},
};
use crossbeam::queue::SegQueue;
use futures::Stream;
use parking_lot::Mutex;
use std::{
    cell::OnceCell,
    collections::HashMap,
    ffi::{c_void, OsString},
    io,
    os::windows::io::{AsRawHandle, RawHandle},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Waker},
    thread::JoinHandle,
};
use tracing::{debug, error, trace};
use windows_sys::{
    core::GUID,
    Win32::{Foundation::*, System::LibraryLoader::GetModuleHandleW, UI::WindowsAndMessaging::*},
};

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
    #[allow(unused)]
    /// The message recipient parameter is a service handle
    /// NOTE this eventually intended to support Service messages (instead of Window messages)
    ///      when service support added we can remove the #[allow(unused)]
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
pub struct Registry(Vec<GUID>);
impl Registry {
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
        self.with(Registry::WCEUSBS)
            .with(Registry::USBDEVICE)
            .with(Registry::PORTS)
    }

    /// Add a GUID to the registration
    pub fn with(mut self, guid: GUID) -> Self {
        self.0.push(guid);
        self
    }

    pub fn spawn<N>(self, n: N) -> ScanResult<WindowEvents>
    where
        N: Into<OsString> + Send + Sync + 'static,
    {
        let name: OsString = n.into();
        let window = name.clone();
        let devices = self::scan()
            .unwrap_or_else(|_| HashMap::new())
            .into_iter()
            .map(|(port, meta)| PlugEvent::Arrival(port, meta))
            .collect();
        let ours = Arc::new(SharedQueue::with_events(devices));
        let theirs = Arc::clone(&ours);
        let join_handle = std::thread::spawn(move || unsafe {
            device_notification_window_dispatcher(name, self, Arc::into_raw(theirs) as _)
        });
        Ok(WindowEvents {
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

#[derive(Debug)]
#[repr(u32)]
pub enum PlugEvent {
    Arrival(OsString, PortMeta) = DBT_DEVICEARRIVAL,
    RemoveComplete(OsString) = DBT_DEVICEREMOVECOMPLETE,
}

#[derive(Default)]
struct SharedQueue {
    queue: SegQueue<Option<ScanResult<PlugEvent>>>,
    waker: Mutex<Option<Waker>>,
}

impl SharedQueue {
    fn with_events(events: Vec<PlugEvent>) -> SharedQueue {
        let queue = SegQueue::new();
        for ev in events {
            queue.push(Some(Ok(ev)));
        }
        SharedQueue {
            queue,
            waker: Mutex::new(None),
        }
    }

    fn try_wake(&self) -> &Self {
        if let Some(waker) = &self.waker.lock().as_ref() {
            waker.wake_by_ref()
        }
        self
    }

    fn try_wake_with(&self, ev: Option<ScanResult<PlugEvent>>) -> &Self {
        self.queue.push(ev);
        self.try_wake();
        self
    }

    fn poll_next(&self, cx: &mut Context<'_>) -> Poll<Option<ScanResult<PlugEvent>>> {
        match self.queue.pop() {
            None => {
                let new_waker = cx.waker();
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
                };
                Poll::Pending
            }
            Some(Some(inner)) => Poll::Ready(Some(inner)),
            Some(None) => Poll::Ready(None),
        }
    }
}

/// A stream of device notifications
pub struct WindowEvents {
    window: OsString,
    context: Arc<SharedQueue>,
    join_handle: Option<JoinHandle<io::Result<()>>>,
}

impl WindowEvents {
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
        let jh = self
            .join_handle
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Already closed WindowEvents"))?;
        jh.join()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "join error"))?
    }
}

impl Drop for WindowEvents {
    fn drop(&mut self) {
        trace!(window=?self.window, "dropping window event");
        match self.close() {
            Ok(_) => trace!(window=?self.window, "WindowEvents drop OK"),
            Err(error) => {
                trace!(window=?self.window, ?error, "WindowEvents drop error")
            }
        }
    }
}

impl Stream for WindowEvents {
    type Item = ScanResult<PlugEvent>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.context.poll_next(cx)
    }
}

/// Creating Windows requires the hinstance prop of the WinMain function. To retreive this
/// parameter use [`windows_sys::Win32::System::LibraryLoader::GetModuleHandleW`];
fn hinstance() -> isize {
    // Safety: If the handle is NULL, GetModuleHandle returns a handle to the file used to create
    // the calling process
    unsafe { GetModuleHandleW(std::ptr::null()) }
}

pub(crate) fn rescan<N>(into_name: N) -> io::Result<()>
where
    N: Into<OsString>,
{
    let name = into_name.into();
    let wide = to_wide(name);
    let hwnd = unsafe {
        let result = FindWindowW(WINDOW_CLASS_NAME, wide.as_ptr());
        match result {
            0 => Err(io::Error::last_os_error()),
            hwnd => Ok(hwnd),
        }
    }?;
    unsafe {
        let result = PostMessageW(hwnd, WM_USER, 0, 0);
        match result {
            0 => Err(io::Error::last_os_error()),
            _ => Ok(()),
        }
    }
}

/// Window proceedure for responding to windows messages and listening for device notifications
unsafe extern "system" fn device_notification_window_proceedure(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const SharedQueue;
    if !ptr.is_null() {
        match msg {
            // Safety: lparam is a DEV_BROADCAST_HDR when msg is WM_DEVICECHANGE
            WM_DEVICECHANGE => match unsafe { parse_event(wparam as _, lparam as _) } {
                Some(msg) => {
                    debug!(?msg);
                    (&*ptr).try_wake_with(Some(msg));
                    0
                }
                None => DefWindowProcW(hwnd, msg, wparam, lparam),
            },
            WM_DESTROY => {
                if let Ok(window) = crate::get_window_text!(hwnd, 128) {
                    trace!(?window, "wm_destroy");
                }
                // NOTE we only reconstruct our arc on destroy
                let arc = Arc::from_raw(ptr as *const SharedQueue);
                arc.try_wake_with(None);
                0
            }
            WM_USER => {
                debug!("received scan request message");
                match hkey::scan() {
                    Ok(map) => {
                        if map.len() > 0 {
                            map.into_iter()
                                .map(|(port, meta)| PlugEvent::Arrival(port, meta))
                                .for_each(|ev| {
                                    (&*ptr).try_wake_with(Some(Ok(ev)));
                                });
                        }
                    }
                    Err(error) => error!(?error, "failed scan"),
                }
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    } else {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

unsafe fn parse_event(ty: u32, data: *mut c_void) -> Option<ScanResult<PlugEvent>> {
    match ty {
        DBT_DEVICEREMOVECOMPLETE => Some(Ok(PlugEvent::RemoveComplete(parse_event_data(data)?))),
        DBT_DEVICEARRIVAL => {
            let port = parse_event_data(data)?;
            match hkey::scan_for(&port) {
                Ok(ids) => Some(Ok(PlugEvent::Arrival(port, ids))),
                Err(e) => Some(Err(e)),
            }
        }
        _ => None,
    }
}

unsafe fn parse_event_data(data: *mut c_void) -> Option<OsString> {
    let broadcast = &mut *(data as *mut DEV_BROADCAST_HDR);
    match broadcast.dbch_devicetype {
        DBT_DEVTYP_PORT => {
            let port = &*(data as *const DEV_BROADCAST_PORT_W);
            Some(wchar::from_wide(port.dbcp_name.as_ptr()))
        }
        _ => None,
    }
}

/// Create an instance of a DeviceNotifier window.
///
/// Safety: name must be a null terminated Wide string, and user_data must be a pointer to an
/// Arc<SharedQueue>;
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
/// Safety: user_data must be a pointer to an Arc<SharedQueue> that was created
/// by Arc::into_raw...
///
/// This method will rebuild the Arc and pass it to the window procedure...
unsafe fn device_notification_window_dispatcher(
    name: OsString,
    registrations: Registry,
    user_data: isize,
) -> io::Result<()> {
    // TODO figure out how to pass atom into class name
    let _atom = get_window_class();
    let unsafe_name = to_wide(name.clone());
    let arc = Arc::from_raw(user_data as *const Arc<SharedQueue>);
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
            _ if msg.message == WM_CLOSE => {
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
