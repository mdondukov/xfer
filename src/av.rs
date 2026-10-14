//! Antivirus check before copying.
//!
//! xfer never changes security settings itself: on managed PCs policy would block
//! it, and an exclusion left behind by an interrupted run would stay forever.
//! Instead it detects real-time scanning and prints the commands to add and remove
//! a temporary exclusion. A process exclusion covers both reads (old PC) and
//! writes (external drive, new PC), unlike a folder exclusion on the target.

use std::path::Path;

#[derive(Debug, Default, PartialEq)]
pub struct Status {
    /// Microsoft Defender real-time protection, if it could be queried.
    pub defender_realtime: Option<bool>,
    /// Other antivirus products registered in Windows Security Center and turned on.
    pub third_party: Vec<String>,
}

/// Prints advice if real-time scanning is likely to slow copying down.
/// Silent when the check is impossible (non-Windows, Windows Server, no PowerShell).
pub fn check() {
    #[cfg(windows)]
    if let Some(out) = windows::query() {
        let exe = std::env::current_exe().unwrap_or_else(|_| "xfer.exe".into());
        if let Some(text) = advice(&parse(&out), &exe) {
            eprintln!("{text}");
        }
    }
}

/// Parses the `KEY=value` lines printed by the PowerShell query.
pub fn parse(out: &str) -> Status {
    let mut st = Status::default();
    for line in out.lines().map(str::trim) {
        if let Some(v) = line.strip_prefix("DEFENDER_RT=") {
            st.defender_realtime = match v.to_ascii_lowercase().as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
        } else if let Some(v) = line.strip_prefix("AV=") {
            let (name, state) = v.rsplit_once('|').unwrap_or((v, ""));
            let name = name.trim();
            // Defender itself is covered by Get-MpComputerStatus.
            if name.is_empty() || name.to_lowercase().contains("defender") {
                continue;
            }
            if state.trim().parse::<u32>().is_ok_and(product_enabled) {
                st.third_party.push(name.to_string());
            }
        }
    }
    st
}

/// Security Center `productState`: bit 12 is set when real-time protection is on
/// (0x061100 = on, 0x060100 = off).
fn product_enabled(state: u32) -> bool {
    (state >> 12) & 1 == 1
}

pub fn advice(st: &Status, exe: &Path) -> Option<String> {
    let exe = crate::paths::display(exe);
    let mut out = String::new();
    if st.defender_realtime == Some(true) {
        out.push_str(&format!(
            "Note: Microsoft Defender real-time protection is on. It scans every file xfer\n\
             reads and writes, which noticeably slows down copying many small files.\n\
             To exclude xfer for the duration of the transfer, run in an admin PowerShell:\n\
             \x20 Add-MpPreference -ExclusionProcess \"{exe}\"\n\
             and remove the exclusion when done:\n\
             \x20 Remove-MpPreference -ExclusionProcess \"{exe}\"\n"
        ));
    }
    if !st.third_party.is_empty() {
        out.push_str(&format!(
            "Note: active antivirus: {}. It scans every file xfer reads and writes.\n\
             For a faster transfer, add {exe} to its exclusions and remove it when done.\n",
            st.third_party.join(", ")
        ));
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(windows)]
mod windows {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    const SCRIPT: &str = r#"
[Console]::OutputEncoding = [Text.Encoding]::UTF8
try { $s = Get-MpComputerStatus -ErrorAction Stop; "DEFENDER_RT=$($s.RealTimeProtectionEnabled)" } catch { "DEFENDER_RT=unknown" }
try {
  Get-CimInstance -Namespace root/SecurityCenter2 -ClassName AntiVirusProduct -ErrorAction Stop |
    ForEach-Object { "AV=$($_.displayName)|$($_.productState)" }
} catch { }
"#;

    /// PowerShell can hang on a broken WMI; give up after a few seconds.
    pub fn query() -> Option<String> {
        let mut child = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", SCRIPT])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut stdout = child.stdout.take()?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
        });
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(out) => {
                let _ = child.wait();
                Some(out)
            }
            Err(_) => {
                let _ = child.kill();
                None
            }
        }
    }

    #[test]
    fn query_runs_on_windows() {
        // GitHub's Windows runners have Defender; we only check the query works.
        let out = query().expect("PowerShell query finished");
        assert!(out.contains("DEFENDER_RT="), "unexpected output: {out}");
        println!("{:?}", super::parse(&out));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_defender_and_third_party() {
        let out = "DEFENDER_RT=False\r\n\
                   AV=Windows Defender|393472\r\n\
                   AV=Kaspersky Endpoint Security|266240\r\n\
                   AV=Old AV|262144\r\n";
        let st = parse(out);
        assert_eq!(st.defender_realtime, Some(false));
        assert_eq!(st.third_party, vec!["Kaspersky Endpoint Security".to_string()]);
    }

    #[test]
    fn advice_only_when_scanning_is_on() {
        let exe = Path::new(r"C:\Tools\xfer.exe");
        assert!(advice(&Status::default(), exe).is_none());
        let on = Status { defender_realtime: Some(true), third_party: vec![] };
        let text = advice(&on, exe).unwrap();
        assert!(text.contains(r#"Add-MpPreference -ExclusionProcess "C:\Tools\xfer.exe""#));
        assert!(text.contains("Remove-MpPreference"));
    }
}
