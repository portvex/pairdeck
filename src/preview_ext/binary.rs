//! Executable / object file summary (ELF, Mach-O incl. universal, PE, ar).

use goblin::Object;

/// Bytes read for a summary (headers, dynamic section and section table).
pub const MAX_BINARY_BYTES: usize = 64 << 20;
/// Libraries / sections listed at most.
const MAX_LIST: usize = 24;

/// True if the bytes start with an executable / object magic we summarize.
pub fn looks_executable(data: &[u8]) -> bool {
    data.starts_with(b"\x7fELF")
        || data.starts_with(&[0xfe, 0xed, 0xfa, 0xce])
        || data.starts_with(&[0xfe, 0xed, 0xfa, 0xcf])
        || data.starts_with(&[0xce, 0xfa, 0xed, 0xfe])
        || data.starts_with(&[0xcf, 0xfa, 0xed, 0xfe])
        || data.starts_with(&[0xca, 0xfe, 0xba, 0xbe])
        || data.starts_with(b"MZ")
        || data.starts_with(b"!<arch>\n")
}

/// Summary as (label, value) rows; list rows use an empty label for continuation.
pub type Rows = Vec<(String, String)>;

fn row(rows: &mut Rows, k: &str, v: impl Into<String>) {
    rows.push((k.to_string(), v.into()));
}

fn list(rows: &mut Rows, title: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    row(rows, title, format!("{}", items.len()));
    for it in items.iter().take(MAX_LIST) {
        row(rows, "", format!("  {it}"));
    }
    if items.len() > MAX_LIST {
        row(rows, "", format!("  ... {} more", items.len() - MAX_LIST));
    }
}

fn macho(rows: &mut Rows, m: &goblin::mach::MachO) {
    let arch = mach_arch(m.header.cputype(), m.header.cpusubtype());
    row(rows, "Architecture", arch);
    row(rows, "Type", goblin::mach::header::filetype_to_str(m.header.filetype));
    row(rows, "Bits", if m.is_64 { "64" } else { "32" });
    if m.entry != 0 {
        row(rows, "Entry", format!("{:#x}", m.entry));
    }
    let stripped = m.symbols.as_ref().is_none_or(|s| s.iter().next().is_none());
    row(rows, "Symbols", if stripped { "stripped" } else { "present" });
    let libs: Vec<String> = m.libs.iter().filter(|l| **l != "self").map(|l| l.to_string()).collect();
    list(rows, "Libraries", &libs);
    let mut secs = Vec::new();
    for seg in m.segments.iter() {
        if let Ok(ss) = seg.sections() {
            for (s, _) in ss {
                let seg_name = s.segname().unwrap_or("?");
                let name = s.name().unwrap_or("?");
                secs.push(format!("{seg_name},{name}  {}", crate::util::human_size(s.size)));
            }
        }
    }
    list(rows, "Sections", &secs);
}

/// Mach-O architecture name, with fallbacks for subtypes goblin does not name (arm64e).
fn mach_arch(cpu: u32, sub: u32) -> &'static str {
    use goblin::mach::constants::cputype::*;
    get_arch_name_from_types(cpu, sub).unwrap_or(match cpu {
        CPU_TYPE_ARM64 if sub & 0xff == 2 => "arm64e",
        CPU_TYPE_ARM64 => "arm64",
        CPU_TYPE_X86_64 => "x86_64",
        CPU_TYPE_X86 => "i386",
        CPU_TYPE_ARM => "arm",
        CPU_TYPE_POWERPC64 => "ppc64",
        CPU_TYPE_POWERPC => "ppc",
        _ => "unknown",
    })
}

/// Build the summary. Errors from malformed files become a single row.
pub fn summary(data: &[u8]) -> Rows {
    let mut rows = Rows::new();
    match Object::parse(data) {
        Ok(Object::Elf(e)) => {
            row(&mut rows, "Format", if e.is_64 { "ELF 64-bit" } else { "ELF 32-bit" });
            row(&mut rows, "Architecture", goblin::elf::header::machine_to_str(e.header.e_machine));
            row(&mut rows, "Type", goblin::elf::header::et_to_str(e.header.e_type));
            row(&mut rows, "Entry", format!("{:#x}", e.entry));
            if let Some(i) = e.interpreter {
                row(&mut rows, "Interpreter", i);
            }
            row(&mut rows, "Symbols", if e.syms.is_empty() { "stripped" } else { "present" });
            let libs: Vec<String> = e.libraries.iter().map(|l| l.to_string()).collect();
            list(&mut rows, "Libraries", &libs);
            let secs: Vec<String> = e
                .section_headers
                .iter()
                .filter_map(|s| {
                    let n = e.shdr_strtab.get_at(s.sh_name)?;
                    (!n.is_empty()).then(|| format!("{n}  {}", crate::util::human_size(s.sh_size)))
                })
                .collect();
            list(&mut rows, "Sections", &secs);
        }
        Ok(Object::Mach(goblin::mach::Mach::Binary(m))) => {
            row(&mut rows, "Format", "Mach-O");
            macho(&mut rows, &m);
        }
        Ok(Object::Mach(goblin::mach::Mach::Fat(fat))) => {
            row(&mut rows, "Format", "Mach-O universal binary");
            let arches: Vec<String> = fat
                .arches()
                .unwrap_or_default()
                .iter()
                .map(|a| {
                    let name = mach_arch(a.cputype, a.cpusubtype);
                    format!("{name}  {}", crate::util::human_size(a.size as u64))
                })
                .collect();
            list(&mut rows, "Slices", &arches);
            if let Ok(goblin::mach::SingleArch::MachO(m)) = fat.get(0) {
                row(&mut rows, "First slice", "");
                macho(&mut rows, &m);
            }
        }
        Ok(Object::PE(pe)) => {
            row(&mut rows, "Format", if pe.is_64 { "PE32+ (64-bit)" } else { "PE32" });
            row(&mut rows, "Architecture", goblin::pe::header::machine_to_str(pe.header.coff_header.machine));
            row(&mut rows, "Type", if pe.is_lib { "DLL" } else { "executable" });
            row(&mut rows, "Entry", format!("{:#x}", pe.entry));
            let libs: Vec<String> = pe.libraries.iter().map(|l| l.to_string()).collect();
            list(&mut rows, "Imports", &libs);
            let secs: Vec<String> = pe
                .sections
                .iter()
                .map(|s| format!("{}  {}", s.name().unwrap_or("?"), crate::util::human_size(s.size_of_raw_data as u64)))
                .collect();
            list(&mut rows, "Sections", &secs);
        }
        Ok(Object::COFF(c)) => {
            row(&mut rows, "Format", "COFF object");
            row(&mut rows, "Architecture", goblin::pe::header::machine_to_str(c.header.machine));
            row(&mut rows, "Sections", c.sections.len().to_string());
        }
        Ok(Object::Archive(a)) => {
            row(&mut rows, "Format", "ar archive (static library)");
            let members: Vec<String> = a.members().iter().map(|m| m.to_string()).collect();
            list(&mut rows, "Members", &members);
        }
        Ok(_) => row(&mut rows, "Format", "unknown"),
        Err(e) => {
            // Truncated or partly corrupt: report what the fixed-size header says.
            if let Some((format, arch)) = header_guess(data) {
                row(&mut rows, "Format", format!("{format} (partial: {e})"));
                row(&mut rows, "Architecture", arch);
            } else {
                row(&mut rows, "Format", format!("unrecognized or truncated ({e})"));
            }
        }
    }
    rows
}

/// Format and architecture from the first header bytes only.
fn header_guess(d: &[u8]) -> Option<(&'static str, String)> {
    let u16le = |o: usize| d.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u16be = |o: usize| d.get(o..o + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
    let u32le = |o: usize| d.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if d.starts_with(b"\x7fELF") {
        let machine = if d.get(5) == Some(&2) { u16be(18) } else { u16le(18) }?;
        let fmt = if d.get(4) == Some(&2) { "ELF 64-bit" } else { "ELF 32-bit" };
        return Some((fmt, goblin::elf::header::machine_to_str(machine).to_string()));
    }
    if d.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) || d.starts_with(&[0xce, 0xfa, 0xed, 0xfe]) {
        let cpu = u32le(4)?;
        let sub = u32le(8)?;
        let arch = mach_arch(cpu, sub);
        return Some(("Mach-O", arch.to_string()));
    }
    if d.starts_with(b"MZ") {
        let pe = u32le(0x3c)? as usize;
        if d.get(pe..pe + 4) == Some(b"PE\0\0") {
            return Some(("PE", goblin::pe::header::machine_to_str(u16le(pe + 4)?).to_string()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_the_test_binary_itself() {
        let exe = std::env::current_exe().unwrap();
        let data = std::fs::read(exe).unwrap();
        assert!(looks_executable(&data));
        let rows = summary(&data);
        let format = rows.iter().find(|(k, _)| k == "Format").map(|(_, v)| v.clone()).unwrap();
        assert!(format.contains("Mach-O") || format.contains("ELF") || format.contains("PE"), "{format}");
        assert!(rows.iter().any(|(k, _)| k == "Architecture"));
    }

    #[test]
    fn garbage_is_reported_not_panicking() {
        assert!(!looks_executable(b"hello"));
        let rows = summary(b"\x7fELF\x02\x01\x01garbage");
        assert_eq!(rows[0].0, "Format");
    }
}
