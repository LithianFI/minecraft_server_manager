use std::path::Path;

use serde::Serialize;

/// Key lines from a crash, shown in the dashboard and the Discord crash message.
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct CrashSummary {
    pub time: i64,
    /// File name inside `crash-reports/`, if the server wrote one
    pub report_file: Option<String>,
    pub description: Option<String>,
    pub exception: Option<String>,
    pub suspected_mods: Option<String>,
}

/// Builds a summary from the newest crash report written at or after `since` (unix seconds),
/// falling back to the last error line in the console log when there is none.
pub fn summarize(server_path: &Path, since: i64, log_lines: &[String]) -> CrashSummary {
    let now = chrono::Utc::now().timestamp();
    let report = newest_report(&server_path.join("crash-reports"), since);

    let mut summary = match &report {
        Some((name, text)) => CrashSummary { report_file: Some(name.clone()), ..parse_report(text) },
        None => CrashSummary::default(),
    };
    summary.time = now;
    if summary.description.is_none() && summary.exception.is_none() {
        summary.exception = last_error_line(log_lines);
    }
    summary
}

/// Reads a crash report by file name, refusing anything outside `crash-reports/`.
pub fn read_report(server_path: &Path, file: &str) -> Result<String, String> {
    if file.contains('/') || file.contains('\\') || file.contains("..") {
        return Err("Invalid crash report name".to_string());
    }
    let bytes = std::fs::read(server_path.join("crash-reports").join(file))
        .map_err(|e| format!("Failed to read crash report: {}", e))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn newest_report(dir: &Path, since: i64) -> Option<(String, String)> {
    let newest = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let modified = e.metadata().ok()?.modified().ok()?;
            let secs = modified.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
            (secs >= since && e.file_name().to_string_lossy().ends_with(".txt")).then_some((secs, e.path()))
        })
        .max_by_key(|(secs, _)| *secs)?;
    let path = newest.1;
    let text = String::from_utf8_lossy(&std::fs::read(&path).ok()?).into_owned();
    Some((path.file_name()?.to_string_lossy().into_owned(), text))
}

fn parse_report(text: &str) -> CrashSummary {
    let mut lines = text.lines().map(str::trim);
    let description = lines
        .by_ref()
        .find_map(|l| l.strip_prefix("Description:"))
        .map(|d| d.trim().to_string());
    // The exception is the first non-empty line after the description
    let exception = description
        .as_ref()
        .and_then(|_| lines.find(|l| !l.is_empty()))
        .map(|l| truncate(l, 300));
    let suspected_mods = text
        .lines()
        .map(str::trim)
        .find_map(|l| {
            l.strip_prefix("Suspected Mods:")
                .or_else(|| l.strip_prefix("Suspected Mod:"))
                .or_else(|| l.strip_prefix("Suspected Mod(s):"))
        })
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty() && m != "NONE");
    CrashSummary { description, exception, suspected_mods, ..Default::default() }
}

fn last_error_line(log_lines: &[String]) -> Option<String> {
    log_lines
        .iter()
        .rev()
        .find(|l| l.contains("/ERROR]") || l.contains("/FATAL]") || l.contains("Exception"))
        .map(|l| truncate(l.trim(), 300))
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT: &str = "---- Minecraft Crash Report ----
// Why did you do that?

Time: 2026-09-23 12:00:00
Description: Exception in server tick loop

java.lang.NullPointerException: Cannot invoke \"Object.toString()\" because \"x\" is null
\tat com.example.Foo.tick(Foo.java:42)

-- Head --
Thread: Server thread
Suspected Mods: Create (create), Flywheel (flywheel)
Stacktrace:
";

    #[test]
    fn parses_crash_report() {
        let s = parse_report(REPORT);
        assert_eq!(s.description.as_deref(), Some("Exception in server tick loop"));
        assert_eq!(
            s.exception.as_deref(),
            Some("java.lang.NullPointerException: Cannot invoke \"Object.toString()\" because \"x\" is null")
        );
        assert_eq!(s.suspected_mods.as_deref(), Some("Create (create), Flywheel (flywheel)"));
    }

    #[test]
    fn ignores_no_suspects() {
        let s = parse_report("Description: Watching Server\n\njava.lang.Error: watchdog\nSuspected Mods: NONE\n");
        assert_eq!(s.suspected_mods, None);
        assert_eq!(s.exception.as_deref(), Some("java.lang.Error: watchdog"));
    }

    #[test]
    fn falls_back_to_log() {
        let lines = vec![
            "[12:00:00] [main/INFO]: Loading mods".to_string(),
            "[12:00:01] [main/ERROR]: Failed to load mod foo: missing dependency bar".to_string(),
            "[12:00:01] [main/INFO]: Stopping".to_string(),
        ];
        assert_eq!(
            last_error_line(&lines).as_deref(),
            Some("[12:00:01] [main/ERROR]: Failed to load mod foo: missing dependency bar")
        );
    }

    #[test]
    fn refuses_path_traversal() {
        assert!(read_report(Path::new("/tmp"), "../../etc/passwd").is_err());
    }
}
