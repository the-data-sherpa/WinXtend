//! Whether a local firewall is likely to be why nobody can see this machine.
//!
//! Discovery failing looks exactly like nothing happening: the agent starts, the
//! log is clean, and no peer ever appears. A host firewall is the one cause of
//! that which the agent can detect and the user can fix, so it is worth naming.
//!
//! # Why this reads a file rather than running `ufw status`
//!
//! `ufw status` refuses to run as anyone but root, and the agent is a per-user
//! process by design — see [`crate::autostart`]. `/etc/ufw/ufw.conf` is
//! world-readable and holds the one fact that matters, so the check costs no
//! privilege and no subprocess.
//!
//! The trade is that the *rules* are not readable: `/etc/ufw/user.rules` is
//! `0640 root:root`. So this can say that ufw is on and it cannot say whether
//! the ports are already allowed. The message is worded as something to check
//! rather than something that is wrong, because claiming a correctly configured
//! machine is broken is worse than saying nothing.
//!
//! Only ufw is checked. It is what Ubuntu ships and what the alpha targets; a
//! machine running nftables or firewalld directly gets no warning, which is the
//! same silence every other Linux tool in this space offers.

use std::path::Path;

/// The one ufw file a non-root process can read.
pub const UFW_CONF: &str = "/etc/ufw/ufw.conf";

/// The mDNS port, which discovery needs in addition to the QUIC listener.
pub const MDNS_PORT: u16 = 5353;

/// Whether `ufw.conf` says the firewall is on.
///
/// A pure function over the file's contents so the parse is testable without a
/// machine that has ufw installed, let alone enabled. `ufw.conf` is shell-style
/// `KEY=value`, so leading whitespace is tolerated and a commented-out line is
/// not a setting.
pub fn ufw_enabled_in(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        // The file documents the setting in a comment that contains the word
        // `yes`, so a substring search over the whole file would answer
        // "enabled" on a machine where it is not.
        !line.starts_with('#')
            && line
                .strip_prefix("ENABLED")
                .and_then(|rest| rest.trim_start().strip_prefix('='))
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("yes"))
    })
}

/// The warning to show, or `None` when there is nothing in the way — including
/// when there is no way to tell, which is every platform but Linux and every
/// Linux machine with no ufw installed.
pub fn warning(quic_port: u16) -> Option<String> {
    warning_from(Path::new(UFW_CONF), quic_port)
}

/// [`warning`] against an arbitrary config path, so the assembled message can be
/// asserted rather than only the parse.
pub fn warning_from(conf: &Path, quic_port: u16) -> Option<String> {
    // Unreadable and absent are the same answer: ufw is not something this
    // machine has, or not something this process can ask about. Neither is worth
    // a warning, and an error about a file the user never mentioned would be
    // noise on every non-Ubuntu desktop.
    let text = std::fs::read_to_string(conf).ok()?;
    ufw_enabled_in(&text).then(|| message(quic_port))
}

/// The wording, which lives here rather than in the UI because it is a pair of
/// shell commands — platform knowledge, not presentation.
fn message(quic_port: u16) -> String {
    format!(
        "ufw is enabled, so check that WinXtend's ports are allowed or peers will \
         never see this machine: `sudo ufw allow {quic_port}/udp` for the agent \
         and `sudo ufw allow {MDNS_PORT}/udp` for discovery. \
         (ufw's rules are readable only by root, so this cannot tell whether they \
         already are.)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real file, verbatim from an Ubuntu 26.04 install, in its off state.
    const UBUNTU_DEFAULT: &str = "\
# /etc/ufw/ufw.conf
#

# Set to yes to start on boot. If setting this remotely, be sure to add a rule
# to allow your remote connection before starting ufw. Eg: 'ufw allow 22/tcp'
ENABLED=no

# Please use the 'ufw' command to set the loglevel. Eg: 'ufw logging medium'.
# See 'man ufw' for details.
LOGLEVEL=low
";

    #[test]
    fn ubuntus_default_is_off_and_earns_no_warning() {
        // The case that matters most: Ubuntu ships ufw disabled, so the common
        // machine must see nothing at all.
        assert!(!ufw_enabled_in(UBUNTU_DEFAULT));
    }

    #[test]
    fn the_comment_explaining_the_setting_is_not_the_setting() {
        // `ENABLED=no` sits directly under a comment containing the word "yes",
        // which is exactly what a substring search gets wrong.
        assert!(!ufw_enabled_in(
            UBUNTU_DEFAULT.replace("ENABLED=no", "").as_str()
        ));
        assert!(!ufw_enabled_in("#ENABLED=yes"));
        assert!(!ufw_enabled_in("# ENABLED=yes"));
    }

    #[test]
    fn an_enabled_firewall_is_recognised_however_it_is_spaced() {
        assert!(ufw_enabled_in("ENABLED=yes"));
        assert!(ufw_enabled_in("  ENABLED = yes  "));
        assert!(ufw_enabled_in("ENABLED=YES"));
        assert!(ufw_enabled_in(
            &UBUNTU_DEFAULT.replace("ENABLED=no", "ENABLED=yes")
        ));
    }

    #[test]
    fn a_setting_that_merely_starts_with_the_word_is_not_it() {
        // `ENABLED_IPV6=yes` is not the same setting, and treating it as one
        // would warn on machines where ufw is off.
        assert!(!ufw_enabled_in("ENABLED_IPV6=yes"));
        assert!(!ufw_enabled_in("DISABLED=yes"));
    }

    #[test]
    fn a_machine_with_no_ufw_is_not_a_machine_with_a_problem() {
        // No file, so nothing to say — the state of every non-Ubuntu desktop and
        // of every other platform.
        let absent =
            std::env::temp_dir().join(format!("winxtend-no-ufw-{}/ufw.conf", std::process::id()));
        assert_eq!(warning_from(&absent, 24800), None);
    }

    #[test]
    fn the_warning_names_both_ports_and_how_to_open_them() {
        // The point of the message is that it can be acted on without going to
        // look anything up, so both commands have to be in it.
        let text = message(24800);
        assert!(text.contains("sudo ufw allow 24800/udp"), "{text}");
        assert!(text.contains("sudo ufw allow 5353/udp"), "{text}");
        // The port is the one actually bound, not the compiled-in default: an
        // agent moved to another port must not print advice for 24800.
        let moved = message(31337);
        assert!(moved.contains("31337/udp"), "{moved}");
        assert!(!moved.contains("24800"), "{moved}");
    }

    #[test]
    fn an_enabled_firewall_produces_the_warning_end_to_end() {
        let dir = std::env::temp_dir().join(format!("winxtend-ufw-on-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("ufw.conf");
        std::fs::write(&conf, "ENABLED=yes\n").unwrap();

        let warning = warning_from(&conf, 24800).expect("an enabled firewall is worth saying");
        assert!(warning.contains("24800/udp"), "{warning}");

        std::fs::write(&conf, UBUNTU_DEFAULT).unwrap();
        assert_eq!(warning_from(&conf, 24800), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
