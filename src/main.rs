use std::fs::{self, File};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use clap::{Parser, Subcommand};

const HWRNG_DIR: &str = "/sys/class/misc/hw_random";
const HWRNG_DEV: &str = "/dev/hwrng";

#[derive(Parser)]
#[command(name = "hwrng", about = "Manage the Linux hwrng subsystem", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Switch the currently active hwrng to the first available NAME.
    ///
    /// Each NAME is matched against /sys/class/misc/hw_random/rng_available:
    /// first an exact match, then a unique prefix match (so `infnoise`
    /// resolves to `infnoise-1-1:1.0`). Multiple NAMEs may be passed as a
    /// preference list in CLI order; the first one that resolves wins.
    Switch {
        /// Names (or unique prefixes) in preference order; the first available is activated.
        #[arg(required = true, num_args = 1.., value_name = "NAME")]
        names: Vec<String>,
    },

    /// List available rngs, marking the active one with `*`.
    List,

    /// Print detailed metadata for every registered rng.
    Info,

    /// Read BYTES of randomness from /dev/hwrng to stdout.
    ///
    /// Raw bytes are written to stdout; pipe to xxd, hexdump, or a file.
    /// Pass --hex to print a hex string instead. Reading /dev/hwrng
    /// requires root on most systems.
    Read {
        /// Number of bytes to read.
        bytes: u64,
        /// Print as a hex string followed by a newline instead of raw bytes.
        #[arg(long)]
        hex: bool,
    },

    /// Keep the highest-priority NAME active, switching whenever it appears.
    ///
    /// Polls rng_available/rng_current and switches whenever a higher-ranked
    /// match is present but not the active one — useful for hot-plugged
    /// devices (unplug + replug) or when multiple instances may come and go.
    /// Each NAME uses the same exact/first-prefix match as `switch`; pass
    /// multiple NAMEs as a preference list in CLI order.
    Watch {
        /// Names (or unique prefixes) in preference order; the highest-ranked available match is kept active.
        #[arg(required = true, num_args = 1.., value_name = "NAME")]
        names: Vec<String>,
        /// Poll interval in seconds.
        #[arg(long, default_value_t = 2.0)]
        interval: f64,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Switch { names } => switch(&names),
        Command::List => list(),
        Command::Info => info(),
        Command::Read { bytes, hex } => read_random(bytes, hex),
        Command::Watch { names, interval } => watch(&names, interval),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hwrng: {e}");
            ExitCode::FAILURE
        }
    }
}

fn switch(queries: &[String]) -> Result<(), String> {
    let dir = Path::new(HWRNG_DIR);
    let available = read_trimmed(&dir.join("rng_available"))?;
    let names: Vec<&str> = available.split_whitespace().collect();

    let target = resolve_preferred(&names, queries)?;

    let current = read_trimmed(&dir.join("rng_current")).unwrap_or_default();
    if current == target {
        println!("hwrng already set to {target}");
        return Ok(());
    }

    fs::write(dir.join("rng_current"), target.as_bytes()).map_err(|e| {
        if e.kind() == io::ErrorKind::PermissionDenied {
            format!("writing rng_current requires root (try sudo): {e}")
        } else {
            format!("failed to write rng_current: {e}")
        }
    })?;

    println!("switched hwrng: {current} -> {target}");
    Ok(())
}

fn watch(queries: &[String], interval_secs: f64) -> Result<(), String> {
    if !(interval_secs.is_finite() && interval_secs > 0.0) {
        return Err(format!("invalid --interval {interval_secs}: must be > 0"));
    }
    let interval = Duration::from_secs_f64(interval_secs);
    let dir = Path::new(HWRNG_DIR);

    let pretty = format_query_list(queries);
    eprintln!("hwrng: watching for {pretty}, polling every {interval_secs}s (Ctrl-C to stop)");

    let mut last_state: Option<String> = None;
    loop {
        let available = read_trimmed(&dir.join("rng_available"))?;
        let current = read_trimmed(&dir.join("rng_current")).unwrap_or_default();
        let names: Vec<&str> = available.split_whitespace().collect();
        let target = first_preferred_match(&names, queries);

        let state = match &target {
            Some(t) if **t == current => format!("ok:{t}"),
            Some(t) => format!("switch:{current}->{t}"),
            None => format!("missing:current={current}"),
        };
        let log_now = last_state.as_deref() != Some(&state);

        match target {
            Some(t) if t != current => {
                if log_now {
                    eprintln!("hwrng: switching {current} -> {t}");
                }
                if let Err(e) = fs::write(dir.join("rng_current"), t.as_bytes()) {
                    if e.kind() == io::ErrorKind::PermissionDenied {
                        return Err(format!("writing rng_current requires root: {e}"));
                    }
                    if log_now {
                        eprintln!("hwrng: switch to {t} failed: {e}");
                    }
                }
            }
            Some(t) => {
                if log_now {
                    eprintln!("hwrng: {pretty} active as {t}");
                }
            }
            None => {
                if log_now {
                    eprintln!("hwrng: no rng matches {pretty} (current: {current})");
                }
            }
        }

        last_state = Some(state);
        thread::sleep(interval);
    }
}

fn first_match<'a>(names: &[&'a str], query: &str) -> Option<&'a str> {
    if let Some(exact) = names.iter().find(|n| **n == query) {
        return Some(*exact);
    }
    names.iter().copied().find(|n| n.starts_with(query))
}

fn first_preferred_match<'a>(names: &[&'a str], queries: &[String]) -> Option<&'a str> {
    queries.iter().find_map(|q| first_match(names, q))
}

fn format_query_list(queries: &[String]) -> String {
    if queries.len() == 1 {
        format!("'{}'", queries[0])
    } else {
        format!("[{}]", queries.join(", "))
    }
}

fn list() -> Result<(), String> {
    let dir = Path::new(HWRNG_DIR);
    let available = read_trimmed(&dir.join("rng_available"))?;
    let current = read_trimmed(&dir.join("rng_current")).unwrap_or_default();

    for name in available.split_whitespace() {
        let marker = if name == current { "*" } else { " " };
        println!("{marker} {name}");
    }
    Ok(())
}

fn info() -> Result<(), String> {
    let dir = Path::new(HWRNG_DIR);
    let available = read_trimmed(&dir.join("rng_available"))?;
    let current = read_trimmed(&dir.join("rng_current")).unwrap_or_default();
    let quality = read_trimmed(&dir.join("rng_quality")).ok();
    let selected = read_trimmed(&dir.join("rng_selected")).ok();

    let names: Vec<&str> = available.split_whitespace().collect();
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            println!();
        }
        if *name == current {
            println!("{name} [active]");
            if let Some(q) = &quality {
                println!("  quality:     {q}/1024 bits");
            }
            if let Some(s) = &selected {
                let label = match s.as_str() {
                    "1" => "user",
                    "0" => "kernel default",
                    other => other,
                };
                println!("  selected:    {label}");
            }
        } else {
            println!("{name}");
        }
        describe_device(name);
    }
    Ok(())
}

fn describe_device(name: &str) {
    if name == "none" {
        println!("  (kernel no-op rng)");
        return;
    }
    if let Some(idx) = name.strip_prefix("tpm-rng-") {
        let path = PathBuf::from(format!("/sys/class/tpm/tpm{idx}"));
        if path.exists() {
            describe_tpm(&path);
            return;
        }
    }
    if let Some((_, suffix)) = name.split_once('-') {
        let usb_intf = PathBuf::from("/sys/bus/usb/devices").join(suffix);
        if usb_intf.exists() {
            describe_usb(&usb_intf, suffix);
            return;
        }
    }
    for bus in ["virtio", "pci", "platform"] {
        if let Some(dev) = find_bus_device(name, bus) {
            describe_bus_device(&dev, bus);
            return;
        }
    }
    println!("  (no device mapping known)");
}

fn find_bus_device(rng_name: &str, bus: &str) -> Option<PathBuf> {
    let drivers_dir = PathBuf::from(format!("/sys/bus/{bus}/drivers"));
    let target = normalize_root(rng_name);
    let target_idx = parse_trailing_index(rng_name);
    for entry in fs::read_dir(&drivers_dir).ok()?.flatten() {
        let drv_name = entry.file_name().to_string_lossy().into_owned();
        if normalize_root(&drv_name) != target {
            continue;
        }
        let mut devs: Vec<PathBuf> = fs::read_dir(entry.path())
            .ok()?
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_symlink()).unwrap_or(false))
            .map(|e| e.path())
            .collect();
        devs.sort();
        let pick = match target_idx {
            Some(i) => devs.into_iter().nth(i),
            None => devs.into_iter().next(),
        };
        return pick.and_then(|p| fs::canonicalize(&p).ok());
    }
    None
}

fn describe_bus_device(path: &Path, bus: &str) {
    println!("  device:      {}", path.display());
    if let Some(d) = readlink_basename(&path.join("driver")) {
        println!("  driver:      {d}");
    }
    if let Some(m) = read_opt(&path.join("modalias")) {
        println!("  modalias:    {m}");
    }
    match bus {
        "pci" => {
            if let Some(bdf) = path.file_name() {
                println!("  pci-bdf:     {}", bdf.to_string_lossy());
            }
            if let Some(v) = read_opt(&path.join("vendor")) {
                println!("  pci-vendor:  {v}");
            }
            if let Some(d) = read_opt(&path.join("device")) {
                println!("  pci-device:  {d}");
            }
            if let Some(c) = read_opt(&path.join("class")) {
                println!("  pci-class:   {c}");
            }
        }
        "virtio" => {
            if let Some(v) = read_opt(&path.join("vendor")) {
                println!("  virtio-vendor: {v}");
            }
            if let Some(d) = read_opt(&path.join("device")) {
                println!("  virtio-device: {d}");
            }
        }
        "platform" => {
            if let Some(of) = read_opt(&path.join("of_node/compatible")) {
                println!("  of-compatible: {of}");
            }
            if let Some(fw) = readlink_basename(&path.join("firmware_node")) {
                println!("  firmware:    {fw}");
            }
        }
        _ => {}
    }
}

fn normalize_root(s: &str) -> String {
    let mut out = s.to_ascii_lowercase().replace(['_', '.'], "-");
    if let Some(idx) = out.rfind('-') {
        if idx > 0 && out[idx + 1..].chars().all(|c| c.is_ascii_digit()) {
            out.truncate(idx);
        }
    }
    loop {
        let trimmed = out
            .strip_suffix("-trng")
            .or_else(|| out.strip_suffix("-hwrng"))
            .or_else(|| out.strip_suffix("-rng"))
            .map(str::to_string);
        match trimmed {
            Some(t) => out = t,
            None => break,
        }
    }
    out
}

fn parse_trailing_index(s: &str) -> Option<usize> {
    let last = s.rsplit(|c: char| c == '.' || c == '-').next()?;
    if !last.is_empty() && last.chars().all(|c| c.is_ascii_digit()) {
        last.parse().ok()
    } else {
        None
    }
}

fn describe_tpm(path: &Path) {
    println!("  device:      {}", path.display());
    if let Some(v) = read_opt(&path.join("tpm_version_major")) {
        println!("  tpm-version: {v}");
    }
    if let Some(d) = readlink_basename(&path.join("device/driver")) {
        println!("  driver:      {d}");
    }
    if let Some(m) = read_opt(&path.join("device/modalias")) {
        println!("  modalias:    {m}");
    }
}

fn describe_usb(intf_path: &Path, suffix: &str) {
    println!("  device:      {}", intf_path.display());
    if let Some(d) = readlink_basename(&intf_path.join("driver")) {
        println!("  driver:      {d}");
    }
    if let Some(m) = read_opt(&intf_path.join("modalias")) {
        println!("  modalias:    {m}");
    }
    let Some(parent_id) = suffix.split(':').next() else {
        return;
    };
    let parent = PathBuf::from("/sys/bus/usb/devices").join(parent_id);
    if !parent.exists() {
        return;
    }
    let vendor = read_opt(&parent.join("idVendor"));
    let manufacturer = read_opt(&parent.join("manufacturer"));
    let product = read_opt(&parent.join("idProduct"));
    let product_name = read_opt(&parent.join("product"));
    match (&vendor, &manufacturer) {
        (Some(v), Some(m)) => println!("  usb-vendor:  {v} ({m})"),
        (Some(v), None) => println!("  usb-vendor:  {v}"),
        _ => {}
    }
    match (&product, &product_name) {
        (Some(p), Some(n)) => println!("  usb-product: {p} ({n})"),
        (Some(p), None) => println!("  usb-product: {p}"),
        _ => {}
    }
    if let Some(s) = read_opt(&parent.join("serial")) {
        println!("  serial:      {s}");
    }
    if let Some(b) = read_opt(&parent.join("bcdDevice")) {
        println!("  bcd-device:  {b}");
    }
}

fn read_opt(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn readlink_basename(link: &Path) -> Option<String> {
    fs::read_link(link)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
}

fn read_random(bytes: u64, hex: bool) -> Result<(), String> {
    let mut stdout = io::stdout().lock();
    if !hex && stdout.is_terminal() {
        return Err(
            "refusing to write binary to a terminal; pass --hex or redirect stdout".into(),
        );
    }

    let mut dev = File::open(HWRNG_DEV).map_err(|e| {
        if e.kind() == io::ErrorKind::PermissionDenied {
            format!("opening {HWRNG_DEV} requires root (try sudo): {e}")
        } else {
            format!("failed to open {HWRNG_DEV}: {e}")
        }
    })?;

    let mut remaining = bytes;
    let mut buf = [0u8; 4096];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        dev.read_exact(&mut buf[..want])
            .map_err(|e| format!("read from {HWRNG_DEV} failed: {e}"))?;
        if hex {
            let mut hex_buf = String::with_capacity(want * 2);
            for b in &buf[..want] {
                use std::fmt::Write as _;
                let _ = write!(hex_buf, "{b:02x}");
            }
            stdout
                .write_all(hex_buf.as_bytes())
                .map_err(|e| format!("write to stdout failed: {e}"))?;
        } else {
            stdout
                .write_all(&buf[..want])
                .map_err(|e| format!("write to stdout failed: {e}"))?;
        }
        remaining -= want as u64;
    }
    if hex {
        stdout
            .write_all(b"\n")
            .map_err(|e| format!("write to stdout failed: {e}"))?;
    }
    Ok(())
}

fn try_resolve(names: &[&str], query: &str) -> Option<String> {
    if let Some(exact) = names.iter().find(|n| **n == query) {
        return Some((*exact).to_string());
    }
    let prefix_matches: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| n.starts_with(query))
        .collect();
    match prefix_matches.as_slice() {
        [] => None,
        [only] => Some((*only).to_string()),
        [first, rest @ ..] => {
            eprintln!(
                "hwrng: '{query}' matches {}, picking {first}",
                std::iter::once(*first)
                    .chain(rest.iter().copied())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            Some((*first).to_string())
        }
    }
}

fn resolve_preferred(names: &[&str], queries: &[String]) -> Result<String, String> {
    for q in queries {
        if let Some(r) = try_resolve(names, q) {
            return Ok(r);
        }
    }
    Err(format!(
        "no rng matches {}. available: {}",
        format_query_list(queries),
        names.join(", ")
    ))
}

fn read_trimmed(path: &Path) -> Result<String, String> {
    fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("failed to read {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{
        first_match, first_preferred_match, normalize_root, parse_trailing_index, resolve_preferred,
    };

    #[test]
    fn normalize_aligns_rng_and_driver_names() {
        // virtio: rng is "virtio_rng.N", driver is "virtio_rng".
        assert_eq!(normalize_root("virtio_rng.0"), "virtio");
        assert_eq!(normalize_root("virtio_rng"), "virtio");
        // platform: rng is "bcm2835", driver is "bcm2835-rng".
        assert_eq!(normalize_root("bcm2835"), "bcm2835");
        assert_eq!(normalize_root("bcm2835-rng"), "bcm2835");
        // pci: rng "intel-rng" / driver "intel-rng".
        assert_eq!(normalize_root("intel-rng"), "intel");
        // omap uses underscore in driver name.
        assert_eq!(normalize_root("omap_rng"), "omap");
        // -trng / -hwrng variants.
        assert_eq!(normalize_root("ingenic-trng"), "ingenic");
        assert_eq!(normalize_root("foo-hwrng"), "foo");
        // tpm path is short-circuited elsewhere; just make sure it normalizes sanely.
        assert_eq!(normalize_root("tpm-rng-0"), "tpm");
    }

    #[test]
    fn trailing_index_is_extracted_from_dot_or_dash() {
        assert_eq!(parse_trailing_index("virtio_rng.0"), Some(0));
        assert_eq!(parse_trailing_index("virtio_rng.3"), Some(3));
        assert_eq!(parse_trailing_index("tpm-rng-2"), Some(2));
        assert_eq!(parse_trailing_index("bcm2835"), None);
        assert_eq!(parse_trailing_index("intel-rng"), None);
    }

    #[test]
    fn first_match_prefers_exact_then_first_prefix() {
        let names = vec!["infnoise-1-1:1.0", "infnoise-2-1:1.0", "tpm-rng-0"];
        assert_eq!(first_match(&names, "infnoise"), Some("infnoise-1-1:1.0"));
        assert_eq!(first_match(&names, "tpm-rng-0"), Some("tpm-rng-0"));
        assert_eq!(first_match(&names, "nope"), None);
    }

    #[test]
    fn resolve_preferred_picks_first_on_ambiguous_prefix() {
        let names = vec!["infnoise-1-1:1.0", "infnoise-2-1:1.0", "tpm-rng-0"];
        let q = |s: &str| vec![s.to_string()];
        // single-match prefix.
        assert_eq!(resolve_preferred(&names, &q("tpm")).unwrap(), "tpm-rng-0");
        // ambiguous prefix → first match.
        assert_eq!(
            resolve_preferred(&names, &q("infnoise")).unwrap(),
            "infnoise-1-1:1.0"
        );
        // exact match wins even when it'd also be a prefix.
        let names2 = vec!["foo", "foobar"];
        assert_eq!(resolve_preferred(&names2, &q("foo")).unwrap(), "foo");
        // no match → error.
        assert!(resolve_preferred(&names, &q("nope")).is_err());
    }

    #[test]
    fn resolve_preferred_falls_back_in_cli_order() {
        let names = vec!["infnoise-1-1:1.0", "tpm-rng-0"];
        let queries = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // first preference unavailable → fall back to second.
        assert_eq!(
            resolve_preferred(&names, &queries(&["nonsense", "tpm"])).unwrap(),
            "tpm-rng-0"
        );
        // first preference available → wins even if later ones also match.
        assert_eq!(
            resolve_preferred(&names, &queries(&["infnoise", "tpm"])).unwrap(),
            "infnoise-1-1:1.0"
        );
        // none match → error.
        assert!(resolve_preferred(&names, &queries(&["x", "y"])).is_err());
    }

    #[test]
    fn first_preferred_match_respects_cli_order() {
        let names = vec!["infnoise-1-1:1.0", "tpm-rng-0"];
        let queries = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // skips unavailable, picks next.
        assert_eq!(
            first_preferred_match(&names, &queries(&["nope", "tpm", "infnoise"])),
            Some("tpm-rng-0")
        );
        // first available wins regardless of later entries.
        assert_eq!(
            first_preferred_match(&names, &queries(&["infnoise", "tpm"])),
            Some("infnoise-1-1:1.0")
        );
        // nothing matches.
        assert_eq!(first_preferred_match(&names, &queries(&["a", "b"])), None);
    }
}
