//! MSFS 2020 and 2024 through SimConnect (Windows only).
//!
//! `SimConnect.dll` is loaded at runtime with `libloading`, as Little Navmap
//! ships it, and only from an absolute path beside the running exe with
//! `LoadLibraryExW(LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32)`,
//! so neither the DLL nor its dependencies can come from the current
//! directory or `PATH` (search-order hijacking). No SDK is needed to build.
//! The connection is read-only: data definitions and requests, never events
//! or writes.
//!
//! - Own aircraft: `RequestDataOnSimObject` once per second with the
//!   changed-only flag and per-datum epsilons, so a parked aircraft sends
//!   almost nothing (the driver adds a heartbeat).
//! - Traffic: `RequestDataOnSimObjectType` every 2 s for aircraft and
//!   helicopters within 80 km; answers arrive one object per message.
//! - Flight or menu: `RequestSystemState("Sim")` every 2 s. In the menus and
//!   while a flight loads there is no user aircraft, so the requests above
//!   get no answers; this one does, which keeps the connection from looking
//!   dead and lets the tray say "no flight loaded" instead of reconnecting.

use std::ffi::{c_char, c_void, CString};
use std::ptr;
use std::time::{Duration, Instant};

use libloading::os::windows::{
    Library as WinLibrary, LOAD_LIBRARY_SEARCH_APPLICATION_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use libloading::Library;

use super::msfs_proto::*;
use super::{Sim, SimError};
use crate::frame::{now_ms, Frame, Own, SimKind};

type Handle = *mut c_void;
type HResult = i32;

type FnOpen = unsafe extern "system" fn(*mut Handle, *const c_char, *mut c_void, u32, *mut c_void, u32) -> HResult;
type FnClose = unsafe extern "system" fn(Handle) -> HResult;
type FnAddToDataDefinition =
    unsafe extern "system" fn(Handle, u32, *const c_char, *const c_char, u32, f32, u32) -> HResult;
type FnRequestDataOnSimObject = unsafe extern "system" fn(Handle, u32, u32, u32, u32, u32, u32, u32, u32) -> HResult;
type FnRequestDataOnSimObjectType = unsafe extern "system" fn(Handle, u32, u32, u32, u32) -> HResult;
type FnGetNextDispatch = unsafe extern "system" fn(Handle, *mut *mut u8, *mut u32) -> HResult;
type FnRequestSystemState = unsafe extern "system" fn(Handle, u32, *const c_char) -> HResult;

const S_OK: HResult = 0;
/// No message at all for this long means the sim went away without a QUIT
/// (the "Sim" state request every 2 s gets an answer while it runs, in a
/// flight and in the menus).
const SILENCE_LIMIT: Duration = Duration::from_secs(15);
/// The same outside a flight: starting up and loading keep SimConnect quiet
/// (no answer at all, seen with MSFS 2024) for longer than a frame-rate
/// hiccup ever does.
const SILENCE_LIMIT_NO_FLIGHT: Duration = Duration::from_secs(120);
/// A connection opened while MSFS 2024 was still starting can stay mute for
/// good, while a fresh one answers at once (seen: 120 s of nothing, then an
/// immediate answer on reconnect). So a connection that has not answered the
/// "Sim" request this long after opening is replaced. A healthy one answers
/// within a poll in a flight and within ~25 s on a loading screen.
const FIRST_ANSWER_LIMIT: Duration = Duration::from_secs(45);

struct Api {
    close: FnClose,
    add_to_data_definition: FnAddToDataDefinition,
    request_data_on_sim_object: FnRequestDataOnSimObject,
    request_data_on_sim_object_type: FnRequestDataOnSimObjectType,
    request_system_state: FnRequestSystemState,
    get_next_dispatch: FnGetNextDispatch,
}

pub struct MsfsSim {
    handle: Handle,
    api: Api,
    hz: u32,
    traffic: TrafficAssembler,
    own: Option<Own>,
    /// The last "Sim" system state: None until MSFS first answers.
    in_flight: Option<bool>,
    next_traffic_request: Instant,
    last_answer: Instant,
    opened_at: Instant,
    // Dropped last: the function pointers above point into it.
    _lib: Library,
}

// The handle is only ever used from the driver thread that owns the value.
unsafe impl Send for MsfsSim {}

fn load(lib: &Library) -> Result<(FnOpen, Api), String> {
    // SAFETY: the symbol types match the SimConnect.h prototypes (stdcall on
    // x86, the system ABI on x64), and the pointers are copied out while the
    // library stays loaded for the lifetime of MsfsSim.
    unsafe {
        let sym = |name: &[u8]| -> Result<*const c_void, String> {
            lib.get::<*const c_void>(name)
                .map(|s| *s)
                .map_err(|e| format!("SimConnect.dll lacks {}: {e}", String::from_utf8_lossy(name)))
        };
        let open: FnOpen = std::mem::transmute(sym(b"SimConnect_Open\0")?);
        let api = Api {
            close: std::mem::transmute::<*const c_void, FnClose>(sym(b"SimConnect_Close\0")?),
            add_to_data_definition: std::mem::transmute::<*const c_void, FnAddToDataDefinition>(sym(
                b"SimConnect_AddToDataDefinition\0",
            )?),
            request_data_on_sim_object: std::mem::transmute::<*const c_void, FnRequestDataOnSimObject>(sym(
                b"SimConnect_RequestDataOnSimObject\0",
            )?),
            request_data_on_sim_object_type: std::mem::transmute::<*const c_void, FnRequestDataOnSimObjectType>(sym(
                b"SimConnect_RequestDataOnSimObjectType\0",
            )?),
            request_system_state: std::mem::transmute::<*const c_void, FnRequestSystemState>(sym(
                b"SimConnect_RequestSystemState\0",
            )?),
            get_next_dispatch: std::mem::transmute::<*const c_void, FnGetNextDispatch>(sym(
                b"SimConnect_GetNextDispatch\0",
            )?),
        };
        Ok((open, api))
    }
}

#[derive(Debug)]
pub enum ConnectError {
    /// No `SimConnect.dll` beside the exe: the install is broken.
    MissingDll(std::path::PathBuf),
    Other(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::MissingDll(p) => write!(f, "{} not found", p.display()),
            ConnectError::Other(s) => f.write_str(s),
        }
    }
}

impl From<String> for ConnectError {
    fn from(s: String) -> Self {
        ConnectError::Other(s)
    }
}

/// Loads `SimConnect.dll` from beside the exe only. Never searches.
fn load_simconnect() -> Result<Library, ConnectError> {
    let exe = std::env::current_exe().map_err(|e| ConnectError::Other(format!("cannot locate the exe: {e}")))?;
    let path = simconnect_path(&exe)
        .ok_or_else(|| ConnectError::Other(format!("exe path {} is not absolute", exe.display())))?;
    if !path.is_file() {
        return Err(ConnectError::MissingDll(path));
    }
    // TODO: verify the Authenticode signature (Microsoft) with WinVerifyTrust
    // before loading. Deferred: it needs the windows-sys WinTrust bindings and
    // a signer check, and the per-user install folder is writable only by the
    // same user who could already replace the exe itself, so it adds little
    // until the app installs per machine.
    //
    // SAFETY: an absolute path, so no search for the DLL itself; the flags
    // limit its dependencies to the app folder and System32. Loading runs
    // SimConnect's DllMain, which only sets up the client library.
    let lib = unsafe {
        WinLibrary::load_with_flags(
            &path,
            LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    }
    .map_err(|e| ConnectError::Other(format!("cannot load {}: {e}", path.display())))?;
    Ok(lib.into())
}

impl MsfsSim {
    /// `hz` is the own-aircraft rate: 1 asks once per second, 2 asks every
    /// 10th visual frame (the pacer trims to 2 Hz). Both keep the CHANGED flag.
    pub fn connect(hz: u32) -> Result<Self, ConnectError> {
        let lib = load_simconnect()?;
        let (open, api) = load(&lib)?;
        let name = CString::new("SkyNexus Connector").expect("no NUL");
        let mut handle: Handle = ptr::null_mut();
        // SAFETY: valid out-pointer and NUL-terminated name; no window or
        // event handle, the connector polls GetNextDispatch.
        let hr = unsafe { open(&mut handle, name.as_ptr(), ptr::null_mut(), 0, ptr::null_mut(), 0) };
        if hr != S_OK || handle.is_null() {
            return Err(ConnectError::Other("not running".into()));
        }
        let mut sim = MsfsSim {
            handle,
            api,
            hz,
            traffic: TrafficAssembler::default(),
            own: None,
            in_flight: None,
            next_traffic_request: Instant::now(),
            last_answer: Instant::now(),
            opened_at: Instant::now(),
            _lib: lib,
        };
        sim.define(DEF_OWN, &OWN_DATUMS)?;
        sim.define(DEF_TRAFFIC, &TRAFFIC_DATUMS)?;
        sim.request_own()?;
        Ok(sim)
    }

    /// (Re)starts the own-aircraft request. Asking again with the same request
    /// id replaces the old one, so this is also how a newly loaded flight's
    /// user aircraft gets picked up.
    fn request_own(&mut self) -> Result<(), String> {
        let (period, interval) = own_request_period(self.hz);
        // SAFETY: open handle, our own definition and request ids.
        let hr = unsafe {
            (self.api.request_data_on_sim_object)(
                self.handle,
                REQ_OWN,
                DEF_OWN,
                OBJECT_ID_USER,
                period,
                DATA_REQUEST_FLAG_CHANGED,
                0,
                interval,
                0,
            )
        };
        if hr != S_OK {
            return Err(format!("RequestDataOnSimObject failed ({hr:#x})"));
        }
        Ok(())
    }

    fn define(&mut self, def: u32, datums: &[Datum]) -> Result<(), String> {
        for d in datums {
            let name = CString::new(d.name).expect("no NUL");
            let units = d.units.map(|u| CString::new(u).expect("no NUL"));
            // SAFETY: open handle; strings live until the call returns.
            let hr = unsafe {
                (self.api.add_to_data_definition)(
                    self.handle,
                    def,
                    name.as_ptr(),
                    units.as_ref().map_or(ptr::null(), |u| u.as_ptr()),
                    d.datatype,
                    d.epsilon,
                    UNUSED,
                )
            };
            if hr != S_OK {
                return Err(format!("AddToDataDefinition {} failed ({hr:#x})", d.name));
            }
        }
        Ok(())
    }

    /// The every-2-s requests: flight or menu, then traffic.
    fn request_periodic(&mut self) -> Result<(), SimError> {
        let state = CString::new(SYSTEM_STATE_SIM).expect("no NUL");
        // SAFETY: open handle, our own request id, NUL-terminated name that
        // lives until the call returns.
        let hr = unsafe { (self.api.request_system_state)(self.handle, REQ_SIM_STATE, state.as_ptr()) };
        if hr != S_OK {
            return Err(SimError::Disconnected);
        }
        for (req, kind) in [
            (REQ_TRAFFIC_AIRCRAFT, SIMOBJECT_TYPE_AIRCRAFT),
            (REQ_TRAFFIC_HELICOPTER, SIMOBJECT_TYPE_HELICOPTER),
        ] {
            // SAFETY: open handle, our own ids, radius inside the API maximum.
            let hr = unsafe {
                (self.api.request_data_on_sim_object_type)(self.handle, req, DEF_TRAFFIC, TRAFFIC_RADIUS_M, kind)
            };
            if hr != S_OK {
                return Err(SimError::Disconnected);
            }
        }
        Ok(())
    }
}

impl Drop for MsfsSim {
    fn drop(&mut self) {
        // SAFETY: the handle came from SimConnect_Open and is closed once.
        unsafe {
            (self.api.close)(self.handle);
        }
    }
}

impl Sim for MsfsSim {
    fn kind(&self) -> SimKind {
        SimKind::Msfs
    }

    fn describe(&self) -> String {
        "MSFS (SimConnect)".into()
    }

    /// Only once MSFS said so. Right after MSFS starts, and while it loads,
    /// SimConnect accepts the connection but answers nothing at all, the
    /// "Sim" request included; in a flight the answer comes within one poll.
    fn flying(&self) -> bool {
        self.in_flight == Some(true)
    }

    fn poll(&mut self) -> Result<Option<Frame>, SimError> {
        let now = Instant::now();
        if now >= self.next_traffic_request {
            self.request_periodic()?;
            self.next_traffic_request = now + TRAFFIC_EVERY;
        }
        let mut own_changed = false;
        let mut snapshot = None;
        for _ in 0..500 {
            let mut data: *mut u8 = ptr::null_mut();
            let mut len: u32 = 0;
            // SAFETY: open handle and valid out-pointers. SimConnect owns the
            // buffer, which stays valid until the next GetNextDispatch call;
            // it is copied out (decoded) before that.
            let hr = unsafe { (self.api.get_next_dispatch)(self.handle, &mut data, &mut len) };
            if hr != S_OK || data.is_null() || len < 12 {
                break;
            }
            // SAFETY: SimConnect reported `len` readable bytes at `data`.
            let msg = unsafe { std::slice::from_raw_parts(data as *const u8, len as usize) };
            let Some(header) = parse_header(msg) else { break };
            // Any message proves the sim is still there.
            self.last_answer = now;
            match header.id {
                RECV_ID_QUIT => return Err(SimError::Disconnected),
                RECV_ID_EXCEPTION => {
                    if let Some((code, send_id, index)) = parse_exception(msg) {
                        log::debug!("simconnect exception {code} (send {send_id}, index {index})");
                    }
                }
                RECV_ID_SIMOBJECT_DATA => {
                    if let Some(od) = parse_object_data(msg) {
                        if od.request_id == REQ_OWN {
                            if let Some(own) = decode_own(od.data) {
                                self.own = Some(own);
                                own_changed = true;
                            }
                        }
                    }
                }
                RECV_ID_SIMOBJECT_DATA_BYTYPE => {
                    if let Some(od) = parse_object_data(msg) {
                        if let Some(s) = self.traffic.push(&od, now) {
                            snapshot = Some(s);
                        }
                    }
                }
                RECV_ID_SYSTEM_STATE => {
                    if let Some((REQ_SIM_STATE, value)) = parse_system_state(msg) {
                        let flying = value == 1;
                        if self.in_flight != Some(flying) {
                            log::debug!("msfs: Sim state {value}");
                            if flying && self.in_flight.is_some() {
                                // Back from the menus: ask for the new user aircraft.
                                self.request_own().map_err(SimError::Other)?;
                            }
                            if !flying {
                                self.own = None;
                                self.traffic = TrafficAssembler::default();
                            }
                            self.in_flight = Some(flying);
                        }
                    }
                }
                _ => {}
            }
        }
        let limit = if self.flying() {
            SILENCE_LIMIT
        } else {
            SILENCE_LIMIT_NO_FLIGHT
        };
        if now.duration_since(self.last_answer) > limit {
            return Err(SimError::Disconnected);
        }
        if self.in_flight.is_none() && now.duration_since(self.opened_at) > FIRST_ANSWER_LIMIT {
            return Err(SimError::Other(format!(
                "MSFS accepted the connection but has not answered in {} s; reconnecting",
                FIRST_ANSWER_LIMIT.as_secs()
            )));
        }
        if !self.flying() {
            self.own = None;
            return Ok(None);
        }
        if !own_changed && snapshot.is_none() {
            return Ok(None);
        }
        let Some(own) = self.own.clone() else {
            return Ok(None);
        };
        let mut f = Frame::new(SimKind::Msfs, now_ms(), own.clone());
        f.traffic = snapshot.map(|t| drop_own_echo(t, &own));
        Ok(Some(f))
    }
}
