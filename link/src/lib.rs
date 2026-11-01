//! From an in-memory object to an executable on disk.
//!
//! Two seams live here, each with one implementation today:
//!
//! * [`ObjectFormat`]: ELF, Mach-O, or COFF, picked from the host.
//! * [`Linker`]: [`HostCc`], the host C toolchain. `lld` can join it later if
//!   a profile says the system linker is the slow part.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub use object;

/// The triple Athrad was built for, which is the one it compiles for.
pub const HOST_TARGET: &str = env!("ATHRAD_HOST_TARGET");

/// The object file formats Athrad writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ObjectFormat {
    Elf,
    MachO,
    Coff,
}

impl ObjectFormat {
    /// The host's native format.
    pub fn host() -> Self {
        Self::for_target(HOST_TARGET).expect("Athrad is built for a host it can target")
    }

    pub fn for_target(triple: &str) -> Option<Self> {
        if triple.contains("-apple-") {
            Some(ObjectFormat::MachO)
        } else if triple.contains("-windows") {
            Some(ObjectFormat::Coff)
        } else if triple.contains("-linux") || triple.contains("-freebsd") {
            Some(ObjectFormat::Elf)
        } else {
            None
        }
    }

    /// File extension for an object file in this format, as the host
    /// toolchain expects it.
    pub fn extension(self) -> &'static str {
        match self {
            ObjectFormat::Coff => "obj",
            ObjectFormat::Elf | ObjectFormat::MachO => "o",
        }
    }

    pub fn as_object(self) -> object::BinaryFormat {
        match self {
            ObjectFormat::Elf => object::BinaryFormat::Elf,
            ObjectFormat::MachO => object::BinaryFormat::MachO,
            ObjectFormat::Coff => object::BinaryFormat::Coff,
        }
    }
}

impl fmt::Display for ObjectFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ObjectFormat::Elf => "ELF",
            ObjectFormat::MachO => "Mach-O",
            ObjectFormat::Coff => "COFF",
        })
    }
}

/// Serialize an object and write it to `path`. Fails if the object is not in
/// `format`, so a backend cannot quietly hand the linker the wrong kind.
pub fn write_object(obj: &object::write::Object<'_>, format: ObjectFormat, path: &Path) -> Result<()> {
    if obj.format() != format.as_object() {
        bail!("object is {:?}, expected {format}", obj.format());
    }
    let bytes = obj.write().map_err(|e| anyhow::anyhow!("serializing {format} object: {e}"))?;
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}

/// The host's executable file name for `stem`: `stem.exe` on Windows.
pub fn executable_name(stem: &str) -> String {
    format!("{stem}{}", std::env::consts::EXE_SUFFIX)
}

/// Turns object files into an executable.
pub trait Linker {
    fn link(&self, objects: &[PathBuf], output: &Path) -> Result<()>;
}

/// The host C toolchain, found the way the `cc` crate finds it: `CC` or `cc`
/// on Unix, and on MSVC `link.exe` with the environment Visual Studio needs.
pub struct HostCc {
    tool: cc::Tool,
    msvc: bool,
}

impl HostCc {
    pub fn new() -> Result<Self> {
        Self::for_target(HOST_TARGET)
    }

    pub fn for_target(target: &str) -> Result<Self> {
        if target.contains("-windows-msvc") {
            let tool = cc::windows_registry::find_tool(target, "link.exe")
                .context("could not find the MSVC linker (link.exe); install the Visual Studio C++ build tools")?;
            return Ok(HostCc { tool, msvc: true });
        }
        let tool = cc::Build::new()
            .target(target)
            .host(target)
            .opt_level(0)
            .debug(false)
            .cargo_metadata(false)
            .cargo_warnings(false)
            .cargo_output(false)
            .try_get_compiler()
            .context("could not find the host C compiler")?;
        Ok(HostCc { tool, msvc: false })
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(self.tool.path());
        for (k, v) in self.tool.env() {
            cmd.env(k, v);
        }
        cmd
    }
}

impl Linker for HostCc {
    fn link(&self, objects: &[PathBuf], output: &Path) -> Result<()> {
        let mut cmd = self.command();
        if self.msvc {
            cmd.arg("/NOLOGO").arg("/SUBSYSTEM:CONSOLE");
            cmd.arg(format!("/OUT:{}", output.display()));
            cmd.args(objects);
            // The dynamic C runtime, as rustc links it by default.
            cmd.args(["msvcrt.lib", "vcruntime.lib", "ucrt.lib", "kernel32.lib"]);
        } else {
            cmd.arg("-o").arg(output);
            cmd.args(objects);
        }
        let out = cmd.output().with_context(|| format!("running linker {}", self.tool.path().display()))?;
        if !out.status.success() {
            bail!(
                "linker {} failed ({})\n{}{}",
                self.tool.path().display(),
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_for_the_three_hosts() {
        assert_eq!(ObjectFormat::for_target("x86_64-unknown-linux-gnu"), Some(ObjectFormat::Elf));
        assert_eq!(ObjectFormat::for_target("aarch64-apple-darwin"), Some(ObjectFormat::MachO));
        assert_eq!(ObjectFormat::for_target("x86_64-pc-windows-msvc"), Some(ObjectFormat::Coff));
        assert_eq!(ObjectFormat::for_target("wasm32-unknown-unknown"), None);
    }

    #[test]
    fn host_format_is_known() {
        let _ = ObjectFormat::host();
    }
}
