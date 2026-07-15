// Phoenix Simulacra launcher.
//
// A tiny arch-selecting shim shipped as ONE binary next to the two GUI builds.
// It is compiled for x86_64 so it runs natively on x64 and under emulation on
// Windows-on-ARM64; at run time it asks the OS which machine it is *really*
// on (`IsWow64Process2`) and launches the matching GUI that sits beside it:
//
//     simulacra.exe        (x64 GUI)   on x64
//     simulacra-arm.exe    (ARM64 GUI) on ARM64
//
// The launcher itself runs unelevated (asInvoker — no shield on its icon). The
// GUI needs administrator, so the launcher elevates it on demand with
// `ShellExecuteW` + the "runas" verb: exactly one UAC prompt, attributed to the
// GUI rather than this shim. Command-line arguments are forwarded to the GUI
// unchanged (e.g. `--page`, `--demo-mounts`).
#![windows_subsystem = "windows"]

use std::env;
use std::path::{Path, PathBuf};

fn main() {
    let gui_name = if is_arm64() {
        "simulacra-arm.exe"
    } else {
        "simulacra.exe"
    };

    // Resolve the GUI next to the launcher itself, not the current directory,
    // so the bundle works no matter where it is invoked from (Start menu,
    // shortcut, another cwd).
    let dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let target = dir.join(gui_name);

    // Forward every argument after argv[0] to the GUI verbatim.
    let args: Vec<String> = env::args().skip(1).collect();

    if let Err(e) = launch(&target, &args) {
        report_error(&format!(
            "Failed to launch the Phoenix Simulacra interface.\n\n{}\n\n{}",
            target.display(),
            e
        ));
    }
}

/// Elevate and start the GUI via ShellExecute's "runas" verb. Returns `Ok` once
/// the GUI has been handed off (fire-and-forget); the launcher then exits.
#[cfg(windows)]
fn launch(target: &Path, args: &[String]) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    // ShellExecute takes the whole argument list as one command-line string, so
    // re-quote each arg the way CommandLineToArgvW will parse it back apart.
    let params: String = args
        .iter()
        .map(|a| quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ");

    let verb = wide("runas");
    let file = wide(&target.to_string_lossy());
    let params_w = wide(&params);
    let dir_w = target
        .parent()
        .map(|p| wide(&p.to_string_lossy()))
        .unwrap_or_else(|| wide(""));

    // SAFETY: all four PCWSTRs are NUL-terminated UTF-16 buffers that outlive
    // the call; a null HWND is valid for a parentless invocation. An empty
    // params string is passed as null so the GUI sees no phantom argument.
    let hinst = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            if params.is_empty() {
                std::ptr::null()
            } else {
                params_w.as_ptr()
            },
            dir_w.as_ptr(),
            SW_SHOWNORMAL,
        )
    };

    // ShellExecuteW encodes success/failure in its returned "HINSTANCE": any
    // value > 32 is success; <= 32 is an SE_ERR_* code.
    const SUCCESS_FLOOR: isize = 32;
    const SE_ERR_ACCESSDENIED: isize = 5; // also what a cancelled UAC returns
    let code = hinst as isize;
    if code > SUCCESS_FLOOR {
        Ok(())
    } else if code == SE_ERR_ACCESSDENIED {
        // The user dismissed the UAC prompt (or lacks rights). That's a
        // deliberate choice, not a fault — exit quietly without a scary dialog.
        std::process::exit(0);
    } else {
        Err(format!("ShellExecute failed (code {code})"))
    }
}

#[cfg(not(windows))]
fn launch(target: &Path, args: &[String]) -> Result<(), String> {
    std::process::Command::new(target)
        .args(args)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Encode a UTF-8 string as a NUL-terminated UTF-16 buffer for the *W Win32 API.
#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Quote a single argument so `CommandLineToArgvW` reconstructs it exactly.
/// Follows the documented MSVC rule set: only backslashes that immediately
/// precede a double quote (or the closing quote) are doubled.
#[cfg(windows)]
fn quote_arg(arg: &str) -> String {
    let needs_quotes = arg.is_empty()
        || arg
            .chars()
            .any(|c| c == ' ' || c == '\t' || c == '\n' || c == '\x0b' || c == '"');
    if !needs_quotes {
        return arg.to_string();
    }

    let chars: Vec<char> = arg.chars().collect();
    let mut out = String::from('"');
    let mut i = 0;
    while i < chars.len() {
        let mut backslashes = 0;
        while i < chars.len() && chars[i] == '\\' {
            backslashes += 1;
            i += 1;
        }
        if i == chars.len() {
            // Escape trailing backslashes so they don't escape the closing quote.
            out.extend(std::iter::repeat('\\').take(backslashes * 2));
        } else if chars[i] == '"' {
            // Escape all preceding backslashes and the quote itself.
            out.extend(std::iter::repeat('\\').take(backslashes * 2 + 1));
            out.push('"');
            i += 1;
        } else {
            out.extend(std::iter::repeat('\\').take(backslashes));
            out.push(chars[i]);
            i += 1;
        }
    }
    out.push('"');
    out
}

/// True when the *native* machine is ARM64, even if this process is an x64
/// binary running under emulation. `IsWow64Process2` reports the native
/// machine directly, which `GetNativeSystemInfo`/`cfg!(target_arch)` cannot do
/// from inside x64-on-ARM emulation.
#[cfg(windows)]
fn is_arm64() -> bool {
    use windows_sys::Win32::System::SystemInformation::IMAGE_FILE_MACHINE_ARM64;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, IsWow64Process2};

    let mut process_machine: u16 = 0;
    let mut native_machine: u16 = 0;
    // SAFETY: `GetCurrentProcess` returns the current-process pseudo-handle and
    // both out-params are valid, initialized `u16`s owned by this frame.
    let ok = unsafe {
        IsWow64Process2(
            GetCurrentProcess(),
            &mut process_machine,
            &mut native_machine,
        )
    };
    ok != 0 && native_machine == IMAGE_FILE_MACHINE_ARM64
}

#[cfg(not(windows))]
fn is_arm64() -> bool {
    // Cross-platform builds (dev tooling / cargo check on non-Windows) never
    // ship; default to the x64 GUI name.
    false
}

/// The launcher is a GUI-subsystem app with no console, so a failed spawn would
/// otherwise vanish silently. Surface it in a message box instead.
#[cfg(windows)]
fn report_error(msg: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

    let body = wide(msg);
    let title = wide("Phoenix Simulacra");
    // SAFETY: both buffers are NUL-terminated UTF-16; a null HWND is valid.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
fn report_error(msg: &str) {
    eprintln!("{msg}");
}
