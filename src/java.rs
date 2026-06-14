use std::path::{Path, PathBuf};

/// Returns the recommended Java major version for a Minecraft version string ("1.X.Y").
pub fn recommended_java(mc_version: &str) -> u32 {
    let mut parts = mc_version.split('.');
    // Skip the leading "1"
    let _ = parts.next();
    let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(21);
    let patch: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);

    match minor {
        0..=16 => 8,
        17 => 17,
        18 | 19 => 17,
        20 if patch < 5 => 17,
        _ => 21, // 1.20.5+, 1.21+
    }
}

/// Returns the major version reported by a java binary, or None if it can't be run.
pub fn java_version(java_bin: &str) -> Option<u32> {
    let out = std::process::Command::new(java_bin)
        .arg("-version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .ok()?;

    // java -version writes to stderr
    let text = String::from_utf8_lossy(&out.stderr);
    parse_version_output(&text)
}

fn parse_version_output(output: &str) -> Option<u32> {
    // Matches: version "21.0.3" or legacy version "1.8.0_292"
    let start = output.find('"')? + 1;
    let rest = &output[start..];
    let end = rest.find('"')?;
    let ver = &rest[..end];

    if let Some(old) = ver.strip_prefix("1.") {
        // Legacy format: "1.8.0_xxx" → 8
        old.split('.').next()?.parse().ok()
    } else {
        // Modern format: "21.0.3" → 21
        ver.split('.').next()?.parse().ok()
    }
}

/// Find a Java binary that matches `target` major version.
/// Returns `None` if the system default `java` is already the right version
/// (meaning no override is needed).
pub fn find_java(target: u32) -> Option<PathBuf> {
    if java_version("java") == Some(target) {
        return None;
    }

    // /usr/lib/jvm/ — standard location on Debian/Ubuntu/Arch/Fedora
    if let Some(p) = search_dir(Path::new("/usr/lib/jvm"), target) {
        return Some(p);
    }

    // SDKMAN: ~/.sdkman/candidates/java/
    if let Some(home) = dirs::home_dir() {
        if let Some(p) = search_sdkman(&home.join(".sdkman/candidates/java"), target) {
            return Some(p);
        }
    }

    // Homebrew on Linux/macOS: /opt/homebrew/opt/openjdk@{ver}/bin/java
    let brew_path = PathBuf::from(format!("/opt/homebrew/opt/openjdk@{}/bin/java", target));
    if brew_path.exists() && java_version(brew_path.to_str().unwrap_or("")) == Some(target) {
        return Some(brew_path);
    }

    None
}

fn search_dir(jvm_dir: &Path, target: u32) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(jvm_dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_lowercase();
            if version_from_dir_name(&name) == Some(target) {
                let bin = e.path().join("bin/java");
                if bin.exists() { Some(bin) } else { None }
            } else {
                None
            }
        })
        .collect();

    // Sort for determinism; pick the first match.
    candidates.sort();
    candidates.into_iter().next()
}

fn search_sdkman(sdkman_java: &Path, target: u32) -> Option<PathBuf> {
    if !sdkman_java.exists() {
        return None;
    }
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(sdkman_java)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name == "current" {
                return None;
            }
            // SDKMAN dirs: "21.0.3-tem", "17.0.9-ms", "21-open"
            let major: u32 = name.split(['.', '-']).next()?.parse().ok()?;
            if major != target {
                return None;
            }
            let bin = e.path().join("bin/java");
            if bin.exists() { Some(bin) } else { None }
        })
        .collect();

    candidates.sort();
    candidates.into_iter().next()
}

fn version_from_dir_name(name: &str) -> Option<u32> {
    // Common prefixes in /usr/lib/jvm/:
    //   java-21-openjdk, java-21-openjdk-amd64, temurin-21, zulu-21,
    //   corretto-21, microsoft-21, graalvm-jdk-21, liberica-21, sapmachine-21
    const PREFIXES: &[&str] = &[
        "java-", "temurin-", "zulu-", "corretto-", "microsoft-",
        "graalvm-jdk-", "liberica-", "sapmachine-", "semeru-",
    ];
    for prefix in PREFIXES {
        if let Some(rest) = name.strip_prefix(prefix) {
            if let Ok(ver) = rest.split(['-', '.']).next().unwrap_or("").parse::<u32>() {
                return Some(ver);
            }
        }
    }
    None
}
