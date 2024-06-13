//! hkey
use super::wchar::from_wide;
use regex::Regex;
use std::{borrow::Cow, collections::HashMap, error, ffi::OsString, fmt, io};
use tracing::trace;
use windows_sys::Win32::{Foundation::ERROR_SUCCESS, System::Registry::*};

#[derive(Debug)]
pub struct UnexpectedRegistryData {
    expect: u32,
    actual: u32,
    data: Vec<u8>,
}

impl UnexpectedRegistryData {
    fn code_to_str(code: u32) -> &'static str {
        match code {
            REG_BINARY => "[BINARY]",
            REG_DWORD => "[DWORD]",
            REG_DWORD_BIG_ENDIAN => "[DWORD_BIG_ENDIAN]",
            REG_QWORD => "[QWORD]",
            REG_SZ => "[SZ]",
            REG_EXPAND_SZ => "[EXPAND_SZ]",
            REG_MULTI_SZ => "[MULTI_SZ]",
            REG_NONE => "[NONE]",
            _ => "unsupported registry value type",
        }
    }

    /// Convert the error back to the original registry data
    pub fn into_registry_data(self) -> RegistryData {
        RegistryData::from_data(self.actual, self.data)
    }
}

impl error::Error for UnexpectedRegistryData {}
impl fmt::Display for UnexpectedRegistryData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let expect = Self::code_to_str(self.expect);
        let actual = Self::code_to_str(self.actual);
        write!(f, "Expected {expect}, found {actual}")
    }
}

impl From<UnexpectedRegistryData> for io::Error {
    fn from(value: UnexpectedRegistryData) -> io::Error {
        io::Error::new(io::ErrorKind::Other, value.to_string())
    }
}

/// Types of data allowed in the registry
///
/// https://learn.microsoft.com/en-us/windows/win32/sysinfo/registry-value-types
#[derive(Debug)]
pub struct RegistryData {
    pub data: Vec<u8>,
    pub ty: u32,
}
impl RegistryData {
    pub fn from_data(ty: u32, data: Vec<u8>) -> Self {
        Self { data, ty }
    }

    pub fn try_into_expanded_os_string(self) -> Result<OsString, UnexpectedRegistryData> {
        match self.ty {
            // Safety: NOTE this is unsound, as the data might not be null terminated.
            //         TODO - make a from_nwide which excepts a len param and use this instead
            REG_SZ => unsafe { Ok(from_wide(self.data.as_ptr() as _)) },
            REG_EXPAND_SZ => todo!("expand the inner string"),
            val => Err(UnexpectedRegistryData {
                expect: REG_EXPAND_SZ,
                actual: val,
                data: self.data,
            }),
        }
    }

    pub fn try_into_os_string(self) -> Result<OsString, UnexpectedRegistryData> {
        match self.ty {
            // Safety: NOTE this is unsound, as the data might not be null terminated.
            //         TODO - make a from_nwide which excepts a len param and use this instead
            REG_EXPAND_SZ | REG_SZ => unsafe { Ok(from_wide(self.data.as_ptr() as _)) },
            val => Err(UnexpectedRegistryData {
                expect: REG_SZ,
                actual: val,
                data: self.data,
            }),
        }
    }

    pub fn try_into_u32(self) -> Result<u32, UnexpectedRegistryData> {
        let mut bytes: [u8; 4] = [0; 4];
        match self.ty {
            REG_DWORD if self.data.len() == 4 => {
                bytes.copy_from_slice(self.data.as_slice());
                Ok(u32::from_le_bytes(bytes))
            }
            REG_DWORD_BIG_ENDIAN if self.data.len() == 4 => {
                bytes.copy_from_slice(self.data.as_slice());
                Ok(u32::from_be_bytes(bytes))
            }
            actual => Err(UnexpectedRegistryData {
                expect: REG_DWORD,
                actual,
                data: self.data,
            }),
        }
    }

    pub fn try_into_u64(self) -> Result<u64, UnexpectedRegistryData> {
        let mut bytes: [u8; 8] = [0; 8];
        match self.ty {
            REG_DWORD if self.data.len() == 8 => {
                bytes.copy_from_slice(self.data.as_slice());
                Ok(u64::from_le_bytes(bytes))
            }
            REG_DWORD_BIG_ENDIAN if self.data.len() == 8 => {
                bytes.copy_from_slice(self.data.as_slice());
                Ok(u64::from_be_bytes(bytes))
            }
            actual => Err(UnexpectedRegistryData {
                expect: REG_QWORD,
                actual,
                data: self.data,
            }),
        }
    }
}

pub struct PredefinedHkey(HKEY);
impl PredefinedHkey {
    pub const LOCAL_MACHINE: PredefinedHkey = Self(HKEY_LOCAL_MACHINE);
}
impl From<PredefinedHkey> for HKEY {
    fn from(value: PredefinedHkey) -> Self {
        value.0
    }
}

/// https://learn.microsoft.com/en-us/windows/win32/api/winreg/nf-winreg-regqueryinfokeyw
#[derive(Default)]
pub struct HkeyInfo {
    /// The number of subkeys in this key
    pub num_subkeys: usize,
    /// The number of values in this key
    pub num_values: usize,
    /// The size of the key's subkey with the longest name
    pub max_subkey_name_len: usize,
    /// The size of the key's longest value name
    pub max_value_name_len: usize,
    /// The size of the longest data component among all the keys values in bytes
    pub max_value_len: usize,
}

/// A subkey within a predefined HKEY
pub struct Hkey(isize);

impl Hkey {
    /// Query the key and populate a [`crate::hkey::HkeyInfo`] struct
    ///
    /// [See also]
    /// (https://learn.microsoft.com/en-us/windows/win32/api/winreg/nf-winreg-regqueryinfokeyw)
    pub fn info(&self) -> io::Result<HkeyInfo> {
        let mut num_subkeys = 0;
        let mut num_values = 0;
        let mut max_subkey_name_len = 0;
        let mut max_value_name_len = 0;
        let mut max_value_len = 0;
        let result = unsafe {
            RegQueryInfoKeyW(
                self.0,                   // hkey
                std::ptr::null_mut(),     // pclass
                std::ptr::null_mut(),     // class_len
                std::ptr::null_mut(),     // reserved
                &mut num_subkeys,         // nsubkeys
                &mut max_subkey_name_len, // max subkey len
                std::ptr::null_mut(),     // max class len
                &mut num_values,          // nvalues
                &mut max_value_name_len,  // max value name len
                &mut max_value_len,       // max value len
                std::ptr::null_mut(),     // sec descriptor
                std::ptr::null_mut(),     // last write time
            )
        };
        match result {
            ERROR_SUCCESS => Ok(HkeyInfo {
                num_subkeys: num_subkeys as _,
                num_values: num_values as _,
                max_subkey_name_len: max_subkey_name_len as _,
                max_value_name_len: max_value_name_len as _,
                max_value_len: max_value_len as _,
            }),
            _ => Err(io::Error::last_os_error()),
        }
    }

    /// Return an iterator of values listed under this registry key
    ///
    /// [See also]
    /// (https://learn.microsoft.com/en-us/windows/win32/api/winreg/nf-winreg-regenumvaluew)
    pub fn into_values(self) -> io::Result<HkeyValueIter> {
        let info = self.info()?;
        Ok(HkeyValueIter {
            hkey: self,
            info,
            index: 0,
        })
    }
}

impl From<Hkey> for HKEY {
    fn from(value: Hkey) -> Self {
        value.0
    }
}

impl Drop for Hkey {
    fn drop(&mut self) {
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

pub struct HkeyValueIter {
    hkey: Hkey,
    info: HkeyInfo,
    index: usize,
}

/// NOTE this is unsound it returns an io::Error but is really a "System error"
///
/// https://learn.microsoft.com/en-us/windows/win32/debug/system-error-codes
impl Iterator for HkeyValueIter {
    type Item = io::Result<(OsString, RegistryData)>;
    fn next(&mut self) -> Option<Self::Item> {
        // Early return when we are empty
        if self.index == self.info.num_values {
            return None;
        }
        // NOTE we seem to require a +1 on certain registries. We add 2 because wide \0000
        let mut value_name_len: u32 = self.info.max_value_name_len as u32 + 2;
        let mut value_name = Vec::with_capacity(value_name_len as _);
        // NOTE we seem to require a +1 on certain registries. We add 2 because wide \0000
        let mut data_len: u32 = self.info.max_value_len as u32 + 2;
        let mut data = Vec::with_capacity(data_len as _);
        let mut ty = 0;
        let status = unsafe {
            RegEnumValueW(
                self.hkey.0,
                self.index as _,
                value_name.as_mut_ptr(),
                &mut value_name_len,
                std::ptr::null(),
                &mut ty,
                data.as_mut_ptr(),
                &mut data_len,
            )
        };
        match status {
            ERROR_SUCCESS => {
                self.index += 1;
                unsafe {
                    // Safety: We allocated worst case buffers and the kernel has initialized
                    // the data pointed to these buffers up to the data length.
                    //
                    // Safety: value_name has been initialized with a wide char string when
                    // RegEnumValueW returns success
                    data.set_len(data_len as _);
                    Some(Ok((
                        from_wide(value_name.as_ptr()),
                        RegistryData::from_data(ty, data),
                    )))
                }
            }
            _ => Some(Err(io::Error::last_os_error())),
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct PortMeta {
    pub vendor: String,
    pub product: String,
}

impl PortMeta {
    pub fn parse_registry(s: &str) -> Option<PortMeta> {
        let re = Regex::new("(vid_|pid_).{4}").unwrap();
        let mut caps: Vec<String> = re
            .find_iter(s)
            .map(|m| m.as_str()[4..].to_string())
            .collect();
        Some(PortMeta {
            product: caps.pop()?,
            vendor: caps.pop()?,
        })
    }

    pub fn matches(&self, vid: &str, pid: &str) -> bool {
        vid == self.vendor.to_lowercase() && pid == self.product.to_lowercase()
    }
}

impl<'v, 'p, V, P> From<(V, P)> for PortMeta
where
    V: Into<Cow<'v, str>>,
    P: Into<Cow<'p, str>>,
{
    fn from((vid, pid): (V, P)) -> Self {
        PortMeta {
            vendor: vid.into().to_string().to_lowercase(),
            product: pid.into().to_string().to_lowercase(),
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum RegistryError {
    #[error("unexpected registry data => {0}")]
    UnexpectedRegistryData(#[from] UnexpectedRegistryData),
    #[error("io error => {0}")]
    Io(#[from] io::Error),
    #[error("unable to parse registry data {0:?}")]
    UnableToParseRegistryData(OsString),
    #[error("com port {0:?} missing from registry")]
    ComPortMissingFromRegistry(OsString),
}

/// Open a subkey associated with a given parent key
pub fn open<K: Into<OsString>>(parent: PredefinedHkey, subkey: K) -> io::Result<Hkey> {
    let name = crate::wchar::to_wide(subkey);
    unsafe {
        let mut key: HKEY = 0;
        match RegOpenKeyExW(
            parent.into(),
            name.as_ptr(),
            0 as _,
            KEY_READ as _,
            &mut key,
        ) {
            ERROR_SUCCESS => Ok(Hkey(key)),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

/// Helper
pub type ScanResult<T> = Result<T, RegistryError>;

/// Scan the USB device registry.
///
/// This routine will perform 2 registry lookups. First scan
/// `SYSTEM\\CurrentControlSet\\Control\\Com Name Arbiter\\Devices` to get a mapping from COM ports
/// to Vendor/Product ID's.
///
/// Then will scan HARDWARE\\DEVICEMAP\\SERIALCOMM registry to get a list of currently connected
/// devices.  Then we have all the information to provide a hashmap of currently connected USB COM
/// ports including the Vendor/Product ID's.
pub fn scan() -> Result<HashMap<OsString, PortMeta>, RegistryError> {
    // We collect all the currently connected COM ports from the registry
    let connected = open(
        PredefinedHkey::LOCAL_MACHINE,
        "HARDWARE\\DEVICEMAP\\SERIALCOMM",
    )?
    .into_values()?
    .map(|value| value?.1.try_into_os_string().map_err(RegistryError::from))
    .collect::<Result<Vec<OsString>, RegistryError>>()?;

    // We collect all the vender and product id's from the registry
    let devices = open(
        PredefinedHkey::LOCAL_MACHINE,
        "SYSTEM\\CurrentControlSet\\Control\\COM Name Arbiter\\Devices",
    )?
    .into_values()?
    .map(|value| {
        let (port, data) = value?;
        let os_str = data.try_into_os_string()?;
        PortMeta::parse_registry(&os_str.to_string_lossy())
            .ok_or_else(|| RegistryError::UnableToParseRegistryData(os_str))
            .map(|meta| (port, meta))
    })
    .collect::<Result<HashMap<OsString, PortMeta>, RegistryError>>()?;

    // Filter the registry map to only list connected devices We loop again because we want to
    // properly capture errors
    Ok(devices
        .into_iter()
        .filter(|(port, _)| connected.contains(&port))
        .collect())
}

/// Scan all the connected usb devices, and return the ID's for a chosen port (if it exists)
pub fn scan_for(port: &OsString) -> Result<PortMeta, RegistryError> {
    trace!(?port, "scanning for usb device");
    self::scan()
        .map(|mut devices| devices.remove(port))?
        .ok_or_else(|| RegistryError::ComPortMissingFromRegistry(port.to_owned()))
}
