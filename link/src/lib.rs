//! From an in-memory object to an executable on disk.
//!
//! Two seams live here, each with one implementation today:
//!
//! * [`ObjectFormat`]: ELF, Mach-O, or COFF, picked from the host.
//! * [`Linker`]: [`HostCc`], the host C toolchain. `lld` can join it later if
//!   a profile says the system linker is the slow part.
//!
//! On MSVC, finding the host toolchain ([`HostCc::new`]) runs the `cc` crate's
//! search for `link.exe`, which takes longer than a link, so find it once.

use std::ffi::OsString;
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

/// The host C toolchain. On Unix this is the compiler `CC` names, or `cc`; on
/// MSVC it is `link.exe` with the environment Visual Studio needs.
pub struct HostCc {
    program: OsString,
    /// Arguments that come before Athrad's own, from a `CC` such as
    /// `ccache cc` or `cc -m64`.
    args: Vec<OsString>,
    envs: Vec<(OsString, OsString)>,
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
            return Ok(Self::from_tool(&tool, true));
        }
        if target == HOST_TARGET {
            // The `cc` crate's probes take longer than a link (48 ms on macOS)
            // and, for the host, only find what this finds without them.
            let (program, args) = unix_compiler(target, |k| std::env::var_os(k));
            return Ok(HostCc { program, args, envs: Vec::new(), msvc: false });
        }
        let tool = cc::Build::new()
            .target(target)
            .host(HOST_TARGET)
            .opt_level(0)
            .debug(false)
            .cargo_metadata(false)
            .cargo_warnings(false)
            .cargo_output(false)
            .try_get_compiler()
            .with_context(|| format!("could not find a C compiler for {target}"))?;
        Ok(Self::from_tool(&tool, false))
    }

    fn from_tool(tool: &cc::Tool, msvc: bool) -> Self {
        let envs = tool.get_envs().map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
        HostCc { program: tool.path().into(), args: Vec::new(), envs, msvc }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        cmd.envs(self.envs.iter().map(|(k, v)| (k, v)));
        cmd
    }
}

/// The C compiler for a host `target` on Unix, as the `cc` crate picks it:
/// the first set of `CC_<target>`, `CC_<target with underscores>`, `HOST_CC`,
/// and `CC`, split on whitespace into a program and leading arguments, or
/// `cc` when none is set.
fn unix_compiler(target: &str, var: impl Fn(&str) -> Option<OsString>) -> (OsString, Vec<OsString>) {
    let target_u = target.replace(['-', '.'], "_");
    let names = [format!("CC_{target}"), format!("CC_{target_u}"), "HOST_CC".to_owned(), "CC".to_owned()];
    let value = names.iter().filter_map(|n| var(n)).find(|v| !v.to_string_lossy().trim().is_empty());
    let Some(value) = value else {
        return ("cc".into(), Vec::new());
    };
    let value = value.to_string_lossy().into_owned();
    let mut words = value.split_whitespace().map(OsString::from);
    let program = words.next().expect("value is not blank");
    (program, words.collect())
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
        let program = Path::new(&self.program).display();
        let out = cmd.output().with_context(|| format!("running linker {program}"))?;
        if !out.status.success() {
            bail!(
                "linker {program} failed ({})\n{}{}",
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

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        |k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| OsString::from(v))
    }

    fn compiler(target: &str, vars: &[(&str, &str)]) -> (OsString, Vec<OsString>) {
        unix_compiler(target, env(vars))
    }

    #[test]
    fn unix_compiler_defaults_to_cc() {
        assert_eq!(compiler("x86_64-unknown-linux-gnu", &[]), ("cc".into(), vec![]));
        assert_eq!(compiler("x86_64-unknown-linux-gnu", &[("CC", "  ")]), ("cc".into(), vec![]));
    }

    #[test]
    fn unix_compiler_takes_the_most_specific_variable() {
        let t = "aarch64-apple-darwin";
        let all = [("CC", "d"), ("HOST_CC", "c"), ("CC_aarch64_apple_darwin", "b"), ("CC_aarch64-apple-darwin", "a")];
        assert_eq!(compiler(t, &all).0, "a");
        assert_eq!(compiler(t, &all[..3]).0, "b");
        assert_eq!(compiler(t, &all[..2]).0, "c");
        assert_eq!(compiler(t, &all[..1]).0, "d");
    }

    #[test]
    fn unix_compiler_splits_wrappers_and_flags() {
        let (program, args) = compiler("x86_64-unknown-linux-gnu", &[("CC", "ccache  clang -m64")]);
        assert_eq!(program, "ccache");
        assert_eq!(args, ["clang", "-m64"]);
    }

    #[test]
    fn host_format_is_known() {
        let _ = ObjectFormat::host();
    }
}
