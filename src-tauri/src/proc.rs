//! Child-process helpers shared by the server-tree, sidecar, and boot paths.

use std::process::Command;

/// Suppress the console window for helper child processes on Windows.
///
/// The shell is a GUI-subsystem app (`windows_subsystem = "windows"`), but
/// every console-subsystem child it spawns (`tar.exe`, `cmd.exe`,
/// `openmaic-node.exe`) gets a fresh visible console by default — piping
/// stdio does not prevent that. `CREATE_NO_WINDOW` runs them silently while
/// keeping exit codes and captured output intact.
pub(crate) fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}
