//! From an in-memory object to an executable on disk.
//!
//! Two seams live here, each with one implementation today:
//!
//! * [`ObjectFormat`]: ELF, Mach-O, or COFF, picked from the host.
//! * [`Linker`]: [`HostCc`], the host C toolchain. `lld` can join it later if
//!   a profile says the system linker is the slow part.
//!
//! On MSVC, finding the host toolchain ([`HostCc::new`]) runs the `cc` crate's
//! search for `link.exe` and its environment, so find it once and reuse it.

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

/// Everything an executable is linked from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inputs {
    /// Object files, linked whole.
    pub objects: Vec<PathBuf>,
    /// Archives (`std`'s rlibs), in the order `rustc` gives them: a crate
    /// before the crates it depends on.
    pub archives: Vec<PathBuf>,
    /// System libraries the archives need, after them.
    pub libs: Vec<NativeLib>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NativeLib {
    pub name: String,
    pub kind: LibKind,
    /// `name` is the file name itself, not a stem for the linker to decorate.
    pub verbatim: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LibKind {
    Dylib,
    Static,
    /// A macOS framework.
    Framework,
}

/// Turns object files and libraries into an executable.
pub trait Linker {
    fn link(&self, inputs: &Inputs, output: &Path) -> Result<()>;
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
    /// The linker command the driver would run, from [`HostCc::resolve`].
    direct: Option<DirectLink>,
}

/// The command a C compiler driver runs to link, with the places for the
/// output and the inputs marked. Running it directly skips the driver's own
/// start-up and its search for the SDK and the C runtime, every link.
#[derive(Clone, Debug)]
struct DirectLink {
    program: OsString,
    args: Vec<Slot>,
}

#[derive(Clone, Debug)]
enum Slot {
    Arg(OsString),
    Output,
    Inputs,
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
            return Ok(HostCc { program, args, envs: Vec::new(), msvc: false, direct: None });
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
        HostCc { program: tool.path().into(), args: Vec::new(), envs, msvc, direct: None }
    }

    /// Ask the driver once which linker command it runs, and run that
    /// directly from now on. Worth it for a process that links again and
    /// again (`athrad watch`): on macOS the driver is 11 ms of each 37 ms
    /// link. Leaves the driver in place when its answer cannot be used.
    /// A no-op on MSVC, where Athrad runs `link.exe` itself.
    pub fn resolve(&mut self) {
        if self.msvc || self.direct.is_some() {
            return;
        }
        self.direct = self.ask_driver();
    }

    fn ask_driver(&self) -> Option<DirectLink> {
        let dir = std::env::temp_dir().join(format!("athrad-link-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        // The driver wants its inputs to exist, not to be objects.
        let input = dir.join("athrad-probe-input.o");
        let output = dir.join("athrad-probe-output");
        std::fs::write(&input, b"").ok()?;
        let out = self.command().arg("-###").arg("-o").arg(&output).arg(&input).output().ok();
        let _ = std::fs::remove_dir_all(&dir);
        let out = out?;
        if !out.status.success() {
            return None;
        }
        // The last line is the linker (or gcc's `collect2`, which runs it).
        let text = String::from_utf8(out.stderr).ok()?;
        let line = text.lines().rev().find(|l| l.trim_start().starts_with('"'))?;
        let words = split_quoted(line)?;
        let (input, output) = (input.to_str()?, output.to_str()?);
        let (mut saw_in, mut saw_out) = (0, 0);
        let mut args = Vec::new();
        for w in &words[1..] {
            args.push(if w == input {
                saw_in += 1;
                Slot::Inputs
            } else if w == output {
                saw_out += 1;
                Slot::Output
            } else {
                Slot::Arg(w.into())
            });
        }
        (saw_in == 1 && saw_out == 1).then(|| DirectLink { program: words[0].clone().into(), args })
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        cmd.envs(self.envs.iter().map(|(k, v)| (k, v)));
        cmd
    }
}

/// Split a command line as clang's and gcc's `-###` print it: words in
/// double quotes, with backslash escapes inside them, or bare words.
fn split_quoted(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut chars = line.trim().chars().peekable();
    while let Some(&c) = chars.peek() {
        if c == ' ' {
            chars.next();
            continue;
        }
        let mut w = String::new();
        if c == '"' {
            chars.next();
            loop {
                match chars.next()? {
                    '"' => break,
                    '\\' => w.push(chars.next()?),
                    c => w.push(c),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ' ' {
                    break;
                }
                w.push(c);
                chars.next();
            }
        }
        words.push(w);
    }
    (!words.is_empty()).then_some(words)
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
    fn link(&self, inputs: &Inputs, output: &Path) -> Result<()> {
        let mut cmd = self.command();
        if self.msvc {
            cmd.arg("/NOLOGO").arg("/SUBSYSTEM:CONSOLE");
            cmd.arg(format!("/OUT:{}", output.display()));
            cmd.args(&inputs.objects).args(&inputs.archives);
            for lib in &inputs.libs {
                match (lib.kind, lib.verbatim) {
                    (LibKind::Framework, _) => bail!("framework `{}` on Windows", lib.name),
                    (_, true) => cmd.arg(&lib.name),
                    (_, false) => cmd.arg(format!("{}.lib", lib.name)),
                };
            }
            // The dynamic C runtime, as rustc links it by default.
            cmd.args(["msvcrt.lib", "vcruntime.lib", "ucrt.lib", "kernel32.lib"]);
        } else {
            let mut ins: Vec<OsString> = Vec::new();
            ins.extend(inputs.objects.iter().chain(&inputs.archives).map(|p| p.clone().into_os_string()));
            for lib in &inputs.libs {
                match (lib.kind, lib.verbatim) {
                    (LibKind::Framework, _) => ins.extend(["-framework".into(), lib.name.clone().into()]),
                    (_, true) => ins.push(format!("-l:{}", lib.name).into()),
                    (_, false) => ins.push(format!("-l{}", lib.name).into()),
                };
            }
            match &self.direct {
                Some(direct) => {
                    cmd = Command::new(&direct.program);
                    cmd.envs(self.envs.iter().map(|(k, v)| (k, v)));
                    for slot in &direct.args {
                        match slot {
                            Slot::Arg(a) => cmd.arg(a),
                            Slot::Output => cmd.arg(output),
                            Slot::Inputs => cmd.args(&ins),
                        };
                    }
                }
                None => {
                    cmd.arg("-o").arg(output).args(&ins);
                }
            }
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
    fn splits_driver_command_lines() {
        let line = r#" "/usr/bin/ld" "-o" "a b" "x\"y" -lc"#;
        assert_eq!(split_quoted(line).unwrap(), ["/usr/bin/ld", "-o", "a b", "x\"y", "-lc"]);
        assert_eq!(split_quoted(r#""unterminated"#), None);
    }

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
