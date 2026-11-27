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
//! - Own aircraft: `RequestDataOnSimObject` with the changed-only flag and
//!   per-datum epsilons, so a parked aircraft sends almost nothing (the
//!   driver adds a heartbeat). Once per second in the air; every 3rd visual
//!   frame while the aircraft is on the ground and slower than 40 kt, where
//!   the driver sends 4 Hz. The request is replaced when that state changes
//!   (with the driver's hysteresis, `rate::RateSelector`), so the long
//!   airborne part of a flight costs no more SimConnect traffic than before.
//! - Traffic: `RequestDataOnSimObjectType` every 2 s for aircraft and
//!   helicopters within 80 km; answers arrive one object per message.
//! - Aircraft title: the `TITLE` string in a definition of its own, once
//!   per second with the changed-only flag (one answer per aircraft loaded),
//!   for the TechLog presence check.
//! - GSX L:vars: their own definition, every 5 s, for the GSX integration.
//!   If the definition is refused the extras stay `None`.
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
use super::rate::{RateMode, RateSelector};
use super::{Sim, SimError, SimExtras};
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
    /// The air rate (`--hz`).
    hz: u32,
    /// Ground or air, from the decoded own-aircraft samples.
    rate: RateSelector,
    /// The mode the current own-aircraft request was made for.
    requested: RateMode,
    traffic: TrafficAssembler,
    own: Option<Own>,
    /// The loaded aircraft's `TITLE`; None outside a flight.
    title: Option<String>,
    /// GSX L:vars; all None until read, or if the definition was refused.
    extras: SimExtras,
    /// False when the GSX definition failed; nothing is requested then.
    gsx_ok: bool,
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
    // Not verified before loading. The SDK's DLL carries no Authenticode
    // signature (MSFS SDK 0.16: checked), so a WinVerifyTrust check cannot
    // work; a check would have to pin its SHA-256 (src-tauri/vendor/README.md)
    // instead. Deferred: the per-user install folder is writable only by the
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
    /// `hz` is the air rate: 1 asks once per second, 2 asks every 11th
    /// visual frame (the pacer trims to 2 Hz). On the ground and slow it asks
    /// every 3rd visual frame whatever `hz` is. All keep the CHANGED flag.
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
            rate: RateSelector::default(),
            requested: RateMode::Air,
            traffic: TrafficAssembler::default(),
            own: None,
            title: None,
            extras: SimExtras::default(),
            gsx_ok: false,
            in_flight: None,
            next_traffic_request: Instant::now(),
            last_answer: Instant::now(),
            opened_at: Instant::now(),
            _lib: lib,
        };
        sim.define(DEF_OWN, &OWN_DATUMS)?;
        sim.define(DEF_TRAFFIC, &TRAFFIC_DATUMS)?;
        sim.define(DEF_TITLE, &TITLE_DATUMS)?;
        sim.request_own()?;
        sim.request_title()?;
        sim.start_gsx();
        Ok(sim)
    }

    /// Defines and requests the GSX L:vars every 5 s. Any failure only
    /// disables them (extras stay None); own-ship reading is unaffected.
    fn start_gsx(&mut self) {
        self.gsx_ok = false;
        if let Err(e) = self.define(DEF_GSX, &GSX_DATUMS) {
            log::debug!("msfs: GSX L:vars unavailable ({e})");
            return;
        }
        // SAFETY: open handle, our own definition and request ids. The
        // interval skips 4 once-per-second events, so one answer per 5 s.
        let hr = unsafe {
            (self.api.request_data_on_sim_object)(
                self.handle,
                REQ_GSX,
                DEF_GSX,
                OBJECT_ID_USER,
                PERIOD_SECOND,
                0,
                0,
                4,
                0,
            )
        };
        if hr != S_OK {
            log::debug!("msfs: GSX L:var request failed ({hr:#x})");
            return;
        }
        self.gsx_ok = true;
    }

    /// (Re)starts the title request: once per second, answered only when the
    /// title changed, and once at once for a new request.
    fn request_title(&mut self) -> Result<(), String> {
        // SAFETY: open handle, our own definition and request ids.
        let hr = unsafe {
            (self.api.request_data_on_sim_object)(
                self.handle,
                REQ_TITLE,
                DEF_TITLE,
                OBJECT_ID_USER,
                PERIOD_SECOND,
                DATA_REQUEST_FLAG_CHANGED,
                0,
                0,
                0,
            )
        };
        if hr != S_OK {
            return Err(format!("RequestDataOnSimObject (title) failed ({hr:#x})"));
        }
        Ok(())
    }

    /// (Re)starts the own-aircraft request for the current rate mode. Asking
    /// again with the same request id replaces the old one, so this is also
    /// how a newly loaded flight's user aircraft gets picked up and how the
    /// rate follows the aircraft between taxi and flight.
    fn request_own(&mut self) -> Result<(), String> {
        let mode = self.rate.mode();
        let (period, interval) = own_request_period(self.hz, mode);
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
        self.requested = mode;
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

    fn aircraft_title(&self) -> Option<String> {
        self.title.clone().filter(|_| self.flying())
    }

    fn extras(&self) -> SimExtras {
        self.extras.clone()
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
                        if od.request_id == REQ_TITLE {
                            self.title = decode_title(od.data);
                        }
                        if od.request_id == REQ_GSX && self.gsx_ok {
                            self.extras = match decode_gsx(od.data) {
                                Some((started, board, total)) => SimExtras {
                                    couatl_started: Some(started),
                                    pax_board: Some(board),
                                    pax_total: Some(total),
                                },
                                None => SimExtras::default(),
                            };
                        }
                        if od.request_id == REQ_OWN {
                            if let Some(own) = decode_own(od.data) {
                                if self.rate.update(&own) != self.requested {
                                    log::debug!("msfs: own-aircraft rate for {:?}", self.rate.mode());
                                    self.request_own().map_err(SimError::Other)?;
                                }
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
                                self.request_title().map_err(SimError::Other)?;
                                self.start_gsx();
                            }
                            if !flying {
                                // The next flight starts at the air rate.
                                self.rate = RateSelector::default();
                                self.own = None;
                                self.title = None;
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
