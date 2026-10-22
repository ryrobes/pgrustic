//! A small libpq pool. The authenticator connection is low privilege.
//! Each tool call checks one out, runs a transaction, and returns it.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::Mutex;

#[repr(C)]
struct PGconn {
    _private: [u8; 0],
}
#[repr(C)]
struct PGresult {
    _private: [u8; 0],
}

const CONNECTION_OK: c_int = 0;
const PGRES_COMMAND_OK: c_int = 1;
const PGRES_TUPLES_OK: c_int = 2;
const PQTRANS_IDLE: c_int = 0;

unsafe extern "C" {
    fn PQconnectdb(conninfo: *const c_char) -> *mut PGconn;
    fn PQstatus(conn: *const PGconn) -> c_int;
    fn PQerrorMessage(conn: *const PGconn) -> *const c_char;
    fn PQfinish(conn: *mut PGconn);
    fn PQexec(conn: *mut PGconn, query: *const c_char) -> *mut PGresult;
    fn PQresultStatus(res: *const PGresult) -> c_int;
    fn PQresultErrorMessage(res: *const PGresult) -> *const c_char;
    fn PQntuples(res: *const PGresult) -> c_int;
    fn PQnfields(res: *const PGresult) -> c_int;
    fn PQfname(res: *const PGresult, field_num: c_int) -> *const c_char;
    fn PQgetvalue(res: *const PGresult, tup_num: c_int, field_num: c_int) -> *const c_char;
    fn PQgetisnull(res: *const PGresult, tup_num: c_int, field_num: c_int) -> c_int;
    fn PQclear(res: *mut PGresult);
    fn PQtransactionStatus(conn: *const PGconn) -> c_int;
}

pub struct Rows {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

pub struct Conn {
    raw: *mut PGconn,
}

// libpq connections are not shared across threads. The pool only hands a
// connection to one caller at a time.
unsafe impl Send for Conn {}

impl Drop for Conn {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { PQfinish(self.raw) };
            self.raw = ptr::null_mut();
        }
    }
}

impl Conn {
    pub fn connect(conninfo: &str) -> Result<Self, String> {
        let info = cstring(conninfo)?;
        let raw = unsafe { PQconnectdb(info.as_ptr()) };
        if raw.is_null() {
            return Err("PQconnectdb returned null".into());
        }
        let conn = Conn { raw };
        if unsafe { PQstatus(conn.raw) } != CONNECTION_OK {
            return Err(conn.error_message());
        }
        Ok(conn)
    }

    pub fn ok(&self) -> bool {
        !self.raw.is_null() && unsafe { PQstatus(self.raw) } == CONNECTION_OK
    }

    fn error_message(&self) -> String {
        if self.raw.is_null() {
            return "connection closed".into();
        }
        unsafe {
            let ptr = PQerrorMessage(self.raw);
            if ptr.is_null() {
                "postgres error".into()
            } else {
                CStr::from_ptr(ptr).to_string_lossy().trim().to_string()
            }
        }
    }

    pub fn exec(&mut self, sql: &str) -> Result<(), String> {
        let res = self.exec_raw(sql)?;
        let status = unsafe { PQresultStatus(res) };
        if status == PGRES_COMMAND_OK || status == PGRES_TUPLES_OK {
            unsafe { PQclear(res) };
            Ok(())
        } else {
            Err(result_error(res))
        }
    }

    pub fn query(&mut self, sql: &str) -> Result<Rows, String> {
        let res = self.exec_raw(sql)?;
        let status = unsafe { PQresultStatus(res) };
        if status != PGRES_TUPLES_OK && status != PGRES_COMMAND_OK {
            return Err(result_error(res));
        }
        let nfields = unsafe { PQnfields(res) };
        let mut columns = Vec::with_capacity(nfields as usize);
        for i in 0..nfields {
            let name = unsafe { PQfname(res, i) };
            let text = if name.is_null() {
                format!("col{i}")
            } else {
                unsafe { CStr::from_ptr(name).to_string_lossy().into_owned() }
            };
            columns.push(text);
        }
        let ntuples = unsafe { PQntuples(res) };
        let mut rows = Vec::with_capacity(ntuples as usize);
        for t in 0..ntuples {
            let mut row = Vec::with_capacity(nfields as usize);
            for f in 0..nfields {
                if unsafe { PQgetisnull(res, t, f) } != 0 {
                    row.push(None);
                } else {
                    let val = unsafe { PQgetvalue(res, t, f) };
                    let text = if val.is_null() {
                        None
                    } else {
                        Some(unsafe { CStr::from_ptr(val).to_string_lossy().into_owned() })
                    };
                    row.push(text);
                }
            }
            rows.push(row);
        }
        unsafe { PQclear(res) };
        Ok(Rows { columns, rows })
    }

    fn exec_raw(&mut self, sql: &str) -> Result<*mut PGresult, String> {
        if !self.ok() {
            return Err(self.error_message());
        }
        let query = cstring(sql)?;
        let res = unsafe { PQexec(self.raw, query.as_ptr()) };
        if res.is_null() {
            Err(self.error_message())
        } else {
            Ok(res)
        }
    }

    fn idle(&self) -> bool {
        !self.raw.is_null() && unsafe { PQtransactionStatus(self.raw) } == PQTRANS_IDLE
    }

    fn settle(&mut self) {
        if self.ok() && !self.idle() {
            let _ = self.exec("ROLLBACK");
        }
    }
}

fn result_error(res: *mut PGresult) -> String {
    let text = unsafe {
        let ptr = PQresultErrorMessage(res);
        let text = if ptr.is_null() {
            "postgres error".into()
        } else {
            CStr::from_ptr(ptr).to_string_lossy().trim().to_string()
        };
        PQclear(res);
        text
    };
    if text.is_empty() {
        "postgres error".into()
    } else {
        text
    }
}

fn cstring(s: &str) -> Result<CString, String> {
    CString::new(s).map_err(|_| "query contains a nul byte".into())
}

pub fn quote_lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `true` when libpq accepted the password. A refused password is `Ok(false)`.
/// A down server is `Err`.
pub fn password_ok(conninfo: &str) -> Result<bool, String> {
    let info = cstring(conninfo)?;
    let raw = unsafe { PQconnectdb(info.as_ptr()) };
    if raw.is_null() {
        return Err("PQconnectdb returned null".into());
    }
    let status = unsafe { PQstatus(raw) };
    let message = unsafe {
        let ptr = PQerrorMessage(raw);
        if ptr.is_null() {
            String::new()
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    };
    unsafe { PQfinish(raw) };
    if status == CONNECTION_OK {
        return Ok(true);
    }
    let lower = message.to_ascii_lowercase();
    if lower.contains("password")
        || lower.contains("authentication")
        || lower.contains("auth")
        || lower.contains("no pg_hba")
    {
        return Ok(false);
    }
    Err(message.trim().to_string())
}

pub struct Pool {
    conninfo: String,
    idle: Mutex<Vec<Conn>>,
    max: usize,
}

impl Pool {
    pub fn new(conninfo: impl Into<String>, max: usize) -> Self {
        Self {
            conninfo: conninfo.into(),
            idle: Mutex::new(Vec::new()),
            max: max.max(1),
        }
    }

    pub fn with<T>(&self, f: impl FnOnce(&mut Conn) -> Result<T, String>) -> Result<T, String> {
        let mut conn = self.checkout()?;
        let result = f(&mut conn);
        conn.settle();
        if conn.ok() {
            self.checkin(conn);
        }
        result
    }

    fn checkout(&self) -> Result<Conn, String> {
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(conn) = idle.pop() {
            if conn.ok() {
                return Ok(conn);
            }
        }
        drop(idle);
        Conn::connect(&self.conninfo)
    }

    fn checkin(&self, conn: Conn) {
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        if idle.len() < self.max {
            idle.push(conn);
        }
    }
}

pub fn conninfo_escape(s: &str) -> Result<String, String> {
    if s.contains('\0') || s.contains('\n') || s.contains('\r') {
        return Err("connection value contains a control character".into());
    }
    Ok(s.replace('\\', "\\\\").replace('\'', "\\'"))
}
