//! Throwaway diagnostic for 0.59.0's Windows spawn. Never merged.
//!
//! Three competing hypotheses for how Chromium on Windows expects the two
//! `--remote-debugging-pipe` transport handles, asked of a real browser on the
//! runner rather than reasoned about from a development host that cannot run the
//! platform:
//!
//! - **H1 — the CRT file-descriptor block.** `content/browser/devtools/
//!   devtools_pipe_handler.cc` turns the two integers it is handed into handles
//!   with `_get_osfhandle(fd)`, so the child's *CRT* must have descriptors 3 and
//!   4 open. The only documented way a parent populates a child's CRT descriptor
//!   table is `STARTUPINFO`'s `lpReserved2` block, which is what libuv (and
//!   therefore node, and therefore puppeteer's `pipe: true`) writes.
//! - **H2 — H1 plus an explicit handle list.** The same block, with
//!   `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` on a `STARTUPINFOEX` naming exactly the
//!   handles the child may inherit. This is the shape the AppContainer spawn
//!   already builds, so if H2 works and H1 does not, the contained and
//!   uncontained paths stay one path.
//! - **H3 — the standard handles.** The two ends as `hStdInput`/`hStdOutput`.
//!   The control: if the browser speaks under H3, the fd numbering is not what
//!   Windows Chromium reads and every conclusion drawn from the unix arm is
//!   wrong.
//!
//! Each hypothesis is scored by the only thing that settles it: a `Browser.
//! getVersion` request written to the parent's end and a NUL-terminated JSON
//! reply read back off it. A browser that starts and says nothing is a failure,
//! which is exactly the failure this transport produces when the handles are
//! wrong.
#![cfg(windows)]

use std::io::{Read, Write};
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
    TerminateProcess, UpdateProcThreadAttribute, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

/// The CRT's own flags for an entry in the `lpReserved2` descriptor table.
const FOPEN: u8 = 0x01;
const FPIPE: u8 = 0x08;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Where a browser is on this runner, asked rather than assumed.
fn find_browser() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Some(root) = std::env::var_os(var) {
            let root = PathBuf::from(root);
            candidates.push(root.join(r"Google\Chrome\Application\chrome.exe"));
            candidates.push(root.join(r"Microsoft\Edge\Application\msedge.exe"));
            candidates.push(root.join(r"Chromium\Application\chrome.exe"));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Make a handle inheritable in place. The pipe ends `std::io::pipe` hands back
/// are not, and a child cannot be given a handle its parent never marked.
fn make_inheritable(h: RawHandle) -> std::io::Result<()> {
    // SAFETY: `h` belongs to a pipe end owned by the caller for this whole call.
    if unsafe { SetHandleInformation(h as HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The `lpReserved2` block: `[i32 count][u8 flags[count]][HANDLE handles[count]]`,
/// packed exactly as the Microsoft CRT reads it back — unaligned handles
/// included, which is what libuv writes and what the CRT expects.
fn crt_block(entries: &[(u8, HANDLE)]) -> Vec<u8> {
    let count = entries.len() as i32;
    let mut block = Vec::with_capacity(4 + entries.len() * (1 + size_of::<usize>()));
    block.extend_from_slice(&count.to_ne_bytes());
    for (flags, _) in entries {
        block.push(*flags);
    }
    for (_, handle) in entries {
        block.extend_from_slice(&(*handle as usize).to_ne_bytes());
    }
    block
}

struct Attrs(LPPROC_THREAD_ATTRIBUTE_LIST);

impl Drop for Attrs {
    fn drop(&mut self) {
        // SAFETY: initialised by the only constructor that builds this type.
        unsafe { DeleteProcThreadAttributeList(self.0) };
    }
}

/// Which of the three shapes a spawn takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shape {
    /// H1: the CRT descriptor block alone.
    CrtBlock,
    /// H2: the CRT descriptor block plus an explicit inheritable handle list.
    CrtBlockAndHandleList,
    /// H3: the two ends as the child's standard input and output.
    StandardHandles,
}

/// One attempt: start the browser under `shape`, ask it its version, and report
/// what came back.
fn attempt(browser: &PathBuf, shape: Shape) -> String {
    let profile = match tempfile::Builder::new().prefix("io-diag-").tempdir() {
        Ok(d) => d,
        Err(e) => return format!("could not make a profile directory: {e}"),
    };
    let log = profile.path().join("browser.log");
    let logfile = match std::fs::File::create(&log) {
        Ok(f) => f,
        Err(e) => return format!("could not make a log file: {e}"),
    };
    if let Err(e) = make_inheritable(logfile.as_raw_handle()) {
        return format!("could not make the log inheritable: {e}");
    }

    // Two pipes. The child reads commands on one and writes messages on the
    // other, so each pipe hands one end to the child and keeps the other.
    let (child_read, mut parent_write) = match std::io::pipe() {
        Ok(p) => p,
        Err(e) => return format!("could not make the command pipe: {e}"),
    };
    let (mut parent_read, child_write) = match std::io::pipe() {
        Ok(p) => p,
        Err(e) => return format!("could not make the message pipe: {e}"),
    };
    for h in [child_read.as_raw_handle(), child_write.as_raw_handle()] {
        if let Err(e) = make_inheritable(h) {
            return format!("could not make a pipe end inheritable: {e}");
        }
    }
    let child_read_h = child_read.as_raw_handle() as HANDLE;
    let child_write_h = child_write.as_raw_handle() as HANDLE;
    let log_h = logfile.as_raw_handle() as HANDLE;

    let cmdline = format!(
        "\"{}\" --remote-debugging-pipe --headless=new --disable-gpu --no-first-run \
         --no-default-browser-check --user-data-dir=\"{}\" about:blank",
        browser.display(),
        profile.path().display()
    );
    let mut cmd = wide(&cmdline);

    let mut block = crt_block(&[
        (0, INVALID_HANDLE_VALUE),
        (FOPEN, log_h),
        (FOPEN, log_h),
        (FOPEN | FPIPE, child_read_h),
        (FOPEN | FPIPE, child_write_h),
    ]);
    let mut inherit = [child_read_h, child_write_h, log_h];

    let mut si = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            cb: size_of::<STARTUPINFOEXW>() as u32,
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: std::ptr::null_mut(),
            hStdOutput: log_h,
            hStdError: log_h,
            ..Default::default()
        },
        lpAttributeList: std::ptr::null_mut(),
    };
    let mut flags = CREATE_UNICODE_ENVIRONMENT;
    let mut _attrs: Option<Attrs> = None;
    let mut buf: Vec<usize>;

    match shape {
        Shape::CrtBlock => {
            si.StartupInfo.cbReserved2 = block.len() as u16;
            si.StartupInfo.lpReserved2 = block.as_mut_ptr();
        }
        Shape::CrtBlockAndHandleList => {
            si.StartupInfo.cbReserved2 = block.len() as u16;
            si.StartupInfo.lpReserved2 = block.as_mut_ptr();

            let mut size: usize = 0;
            // SAFETY: the documented sizing call; its failure is how the size is
            // reported, so the return value is deliberately not checked.
            unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
            if size == 0 {
                return "the attribute list reported a size of zero".into();
            }
            buf = vec![0usize; size.div_ceil(size_of::<usize>())];
            let list =
                buf.as_mut_ptr().cast::<core::ffi::c_void>() as LPPROC_THREAD_ATTRIBUTE_LIST;
            // SAFETY: `list` points at `buf`, which outlives the spawn below.
            if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) } == 0 {
                return format!("the attribute list would not initialise: {}", last());
            }
            _attrs = Some(Attrs(list));
            // SAFETY: the constant names the handle-array type `inherit` is, the
            // size is that array's own, and it lives until after the spawn.
            if unsafe {
                UpdateProcThreadAttribute(
                    list,
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    inherit.as_mut_ptr().cast(),
                    size_of_val(&inherit),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            } == 0
            {
                return format!("the handle list would not attach: {}", last());
            }
            si.lpAttributeList = list;
            flags |= EXTENDED_STARTUPINFO_PRESENT;
        }
        Shape::StandardHandles => {
            si.StartupInfo.hStdInput = child_read_h;
            si.StartupInfo.hStdOutput = child_write_h;
            si.StartupInfo.hStdError = log_h;
        }
    }

    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: every pointer above is live for the call, and inheritance is on so
    // the handles named reach the child.
    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            flags,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::from_mut(&mut si).cast::<STARTUPINFOW>(),
            &mut pi,
        )
    };
    if ok == 0 {
        return format!("CreateProcessW failed: {}", last());
    }

    // The child owns its ends now. Holding them here would mean never seeing the
    // browser close its output, which reads as a hang rather than an exit.
    drop(child_read);
    drop(child_write);
    drop(logfile);

    let answer = ask(&mut parent_write, &mut parent_read);

    // SAFETY: handles this call has owned since `CreateProcessW` returned.
    unsafe {
        TerminateProcess(pi.hProcess, 1);
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread);
    }

    let complaint = std::fs::read_to_string(&log).unwrap_or_default();
    let complaint = complaint.trim();
    if complaint.is_empty() {
        answer
    } else {
        format!("{answer} | the browser said: {complaint:?}")
    }
}

/// Write one CDP request and read one NUL-terminated reply, bounded.
///
/// The read runs on its own thread because these are anonymous pipes and a
/// blocking read on a browser that never answers is the failure being measured,
/// not a reason to hang the job.
fn ask(w: &mut std::io::PipeWriter, r: &mut std::io::PipeReader) -> String {
    let request = br#"{"id":1,"method":"Browser.getVersion"}"#;
    if let Err(e) = w.write_all(request).and_then(|()| w.write_all(&[0])) {
        return format!("FAIL: could not write to the pipe: {e}");
    }
    if let Err(e) = w.flush() {
        return format!("FAIL: could not flush the pipe: {e}");
    }

    let mut reader = match r.try_clone() {
        Ok(c) => c,
        Err(e) => return format!("FAIL: could not clone the read end: {e}"),
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match reader.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    if byte[0] == 0 {
                        break;
                    }
                    seen.push(byte[0]);
                    if seen.len() > 4096 {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                    return;
                }
            }
        }
        let _ = tx.send(Ok(String::from_utf8_lossy(&seen).into_owned()));
    });

    match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok(text)) if text.contains("product") || text.contains("result") => {
            format!("SPEAKS: {text}")
        }
        Ok(Ok(text)) if text.is_empty() => "FAIL: the pipe closed with nothing on it".into(),
        Ok(Ok(text)) => format!("FAIL: unexpected reply {text:?}"),
        Ok(Err(e)) => format!("FAIL: reading the pipe failed: {e}"),
        Err(_) => "FAIL: nothing was said within 20s".into(),
    }
}

fn last() -> String {
    std::io::Error::last_os_error().to_string()
}

// The CRT's own descriptor lookup, declared rather than depended on: `libc` is a
// unix-only dependency of this crate, and the symbol lives in the UCRT every
// `windows-msvc` binary already links.
extern "C" {
    fn _get_osfhandle(fd: i32) -> isize;
}

/// The half of the question a browser cannot answer.
///
/// Chromium is an MSVC-CRT program, so `_get_osfhandle(3)` works there if the
/// parent wrote an `lpReserved2` block. **This crate's own browser fixture is a
/// Rust binary**, and Rust's standard library uses raw handles and never
/// populates that table itself — so whether a Rust child sees descriptors 3 and 4
/// depends on the UCRT's startup parsing the block on its behalf. If it does not,
/// `tests/browser.rs` cannot run on Windows against the fixture however well the
/// real browser works, and the release finds that out here rather than after the
/// transport is written.
#[test]
fn whether_a_rust_child_sees_the_crt_descriptors_its_parent_passed() {
    // The child arm: re-entered by the spawn below, never by the harness.
    if std::env::var_os("IO_HARNESS_DIAG_CHILD").is_some() {
        return;
    }

    let exe = std::env::current_exe().expect("the test binary has a path");
    let (child_read, mut parent_write) = std::io::pipe().expect("command pipe");
    let (mut parent_read, child_write) = std::io::pipe().expect("message pipe");
    for h in [child_read.as_raw_handle(), child_write.as_raw_handle()] {
        make_inheritable(h).expect("inheritable");
    }
    let log = std::env::temp_dir().join(format!("io-diag-child-{}.txt", std::process::id()));
    let logfile = std::fs::File::create(&log).expect("log");
    make_inheritable(logfile.as_raw_handle()).expect("inheritable log");

    let mut block = crt_block(&[
        (0, INVALID_HANDLE_VALUE),
        (FOPEN, logfile.as_raw_handle() as HANDLE),
        (FOPEN, logfile.as_raw_handle() as HANDLE),
        (FOPEN | FPIPE, child_read.as_raw_handle() as HANDLE),
        (FOPEN | FPIPE, child_write.as_raw_handle() as HANDLE),
    ]);
    let mut cmd = wide(&format!(
        "\"{}\" diag_child_arm_reports_its_descriptors --exact --nocapture \
         --test-threads=1",
        exe.display()
    ));
    let mut env: Vec<u16> = "IO_HARNESS_DIAG_CHILD=1\0\0".encode_utf16().collect();

    let mut si = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            cb: size_of::<STARTUPINFOEXW>() as u32,
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: std::ptr::null_mut(),
            hStdOutput: logfile.as_raw_handle() as HANDLE,
            hStdError: logfile.as_raw_handle() as HANDLE,
            cbReserved2: block.len() as u16,
            lpReserved2: block.as_mut_ptr(),
            ..Default::default()
        },
        lpAttributeList: std::ptr::null_mut(),
    };
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: every pointer is live for the call and inheritance is on.
    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            CREATE_UNICODE_ENVIRONMENT,
            env.as_mut_ptr().cast(),
            std::ptr::null(),
            std::ptr::from_mut(&mut si).cast::<STARTUPINFOW>(),
            &mut pi,
        )
    };
    assert!(ok != 0, "CreateProcessW failed: {}", last());
    drop(child_read);
    drop(child_write);
    drop(logfile);

    // The child arm below writes what it saw and echoes one message back.
    let answer = ask(&mut parent_write, &mut parent_read);
    // SAFETY: handles owned since `CreateProcessW` returned.
    unsafe {
        TerminateProcess(pi.hProcess, 1);
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread);
    }
    let said = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    println!("\n  a Rust child's view of the descriptors: {answer}\n  it printed: {said:?}");
}

/// The child arm of the probe above, run only in the re-entered process.
#[test]
fn diag_child_arm_reports_its_descriptors() {
    if std::env::var_os("IO_HARNESS_DIAG_CHILD").is_none() {
        return;
    }
    // SAFETY: a CRT lookup with no invariants beyond the descriptor number.
    let (three, four) = unsafe { (_get_osfhandle(3), _get_osfhandle(4)) };
    println!("child: _get_osfhandle(3)={three:#x} _get_osfhandle(4)={four:#x}");
    if three == -1 || four == -1 {
        return;
    }
    // Echo whatever arrives on 3 back out on 4, NUL-framed, so the parent's
    // ordinary reader sees a reply exactly as it would from a browser.
    use std::os::windows::io::FromRawHandle;
    // SAFETY: the CRT handed back handles this process owns for its lifetime.
    let (mut r, mut w) = unsafe {
        (
            std::fs::File::from_raw_handle(three as RawHandle),
            std::fs::File::from_raw_handle(four as RawHandle),
        )
    };
    let mut seen = Vec::new();
    let mut byte = [0u8; 1];
    while let Ok(1) = r.read(&mut byte) {
        if byte[0] == 0 {
            break;
        }
        seen.push(byte[0]);
    }
    let _ = w.write_all(br#"{"result":{"product":"rust-child"},"echo":"#);
    let _ = w.write_all(&seen);
    let _ = w.write_all(b"}\0");
    let _ = w.flush();
}

#[test]
fn how_chromium_on_windows_expects_its_two_transport_handles() {
    let browser = find_browser().expect(
        "no Chromium-family browser on this runner — the probe has nothing to ask, \
         which is itself a finding to record before anything is written down",
    );
    let mut report = format!("\n  browser: {}", browser.display());
    for shape in [
        Shape::CrtBlock,
        Shape::CrtBlockAndHandleList,
        Shape::StandardHandles,
    ] {
        let outcome = attempt(&browser, shape);
        report.push_str(&format!("\n  {shape:?}: {outcome}"));
    }
    // Printed whether or not it passes: the table is the result.
    println!("{report}");
    assert!(
        report.contains("SPEAKS"),
        "no shape made the browser speak:{report}"
    );
}
