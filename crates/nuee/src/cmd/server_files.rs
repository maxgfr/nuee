//! The files `nuee group host` writes: an install script that makes a Linux server a keeper of
//! the group, the same script as cloud-init user data, and a README.
//!
//! The script carries the server's key, so the files are as secret as the key itself.

use std::fmt::Write as _;

/// Where the script downloads the release from.
const RELEASES: &str = "https://github.com/maxgfr/nuee/releases/download";

/// The key as `printf` octal escapes: POSIX `sh` writes raw bytes with nothing else installed.
fn octal(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "\\{b:03o}");
        s
    })
}

/// A shell word in single quotes.
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The install script for a server that keeps the group of `link` with `key` as its identity.
pub fn install_script(link: &str, key: &[u8; 32], version: &str, name: &str) -> String {
    format!(
        r#"#!/bin/sh
# Makes this Linux server a keeper of a Nuée group: it coordinates the group when the keepers
# before it are off, keeps the group's files and sites, and starts again after a reboot.
# Written by `nuee group host` for the machine named {name}.
#
# Run it as root on Ubuntu 22.04 or later, Debian 12 or later (x86_64 or arm64):
#   sh install.sh
# or paste cloud-init.yaml as the user data of a new server.
#
# This file holds the server's key: keep it as secret as a password, and delete it once the
# server runs. To revoke the server: nuee group remove <link> <its id>
set -eu

VERSION={version}
case "$(uname -m)" in
  x86_64 | amd64) TARGET=x86_64-unknown-linux-gnu ;;
  aarch64 | arm64) TARGET=aarch64-unknown-linux-gnu ;;
  *) echo "nuee: no release for $(uname -m)" >&2; exit 1 ;;
esac
if [ "$(id -u)" != 0 ]; then
  echo "nuee: run this script as root" >&2
  exit 1
fi
command -v curl >/dev/null || {{ echo "nuee: curl is needed" >&2; exit 1; }}

BASE={releases}/v$VERSION
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
curl -fsSL -o "$TMP/nuee-$TARGET" "$BASE/nuee-$TARGET"
curl -fsSL -o "$TMP/SHA256SUMS" "$BASE/SHA256SUMS"
(cd "$TMP" && grep "  nuee-$TARGET\$" SHA256SUMS | sha256sum -c -)
install -m 0755 "$TMP/nuee-$TARGET" /usr/local/bin/nuee

id nuee >/dev/null 2>&1 || useradd --system --home-dir /var/lib/nuee --shell /usr/sbin/nologin nuee
install -d -m 0700 -o nuee -g nuee /var/lib/nuee /var/lib/nuee/config /var/lib/nuee/data
umask 077
printf '{key}' >/var/lib/nuee/config/secret.key
printf '%s\n' {link} >/var/lib/nuee/group-link
chown nuee:nuee /var/lib/nuee/config/secret.key /var/lib/nuee/group-link

cat >/etc/systemd/system/nuee-group.service <<'UNIT'
[Unit]
Description=Nuée group keeper
Documentation=https://maxgfr.github.io/nuee/docs/#groups
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
User=nuee
Group=nuee
Environment=NUEE_CONFIG_DIR=/var/lib/nuee/config NUEE_DATA_DIR=/var/lib/nuee/data HOME=/var/lib/nuee NUEE_LOG=info
# The link is read from a file, so that it never shows in the process list.
StandardInput=file:/var/lib/nuee/group-link
ExecStart=/usr/local/bin/nuee group up -
Restart=always
RestartSec=5s
TimeoutStopSec=30s
ReadWritePaths=/var/lib/nuee
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
NoNewPrivileges=yes
CapabilityBoundingSet=
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
SystemCallArchitectures=native
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
UMask=0077

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable --now nuee-group
echo "nuee: this server keeps the group now. Follow it with: journalctl -u nuee-group -f"
"#,
        key = octal(key),
        link = quoted(link),
        releases = RELEASES,
    )
}

/// The script as cloud-init user data: pasted when a server is created, it runs once at boot.
pub fn cloud_init(script: &str) -> String {
    let mut out = String::from(
        "#cloud-config\n\
         # Paste this as the user data of a new Linux server (Ubuntu 22.04 or later, Debian 12 or\n\
         # later). It installs Nuée at the first boot and makes the server a keeper of the group.\n\
         # It holds the server's key: keep it secret.\n\
         write_files:\n  - path: /root/nuee-install.sh\n    permissions: '0700'\n    content: |\n",
    );
    for line in script.lines() {
        if line.is_empty() {
            out.push('\n');
        } else {
            let _ = writeln!(out, "      {line}");
        }
    }
    out.push_str("runcmd:\n  - [sh, /root/nuee-install.sh]\n  - [rm, -f, /root/nuee-install.sh]\n");
    out
}

/// What the folder is and how to use it.
pub fn readme(name: &str, id: &str) -> String {
    format!(
        "An always-on keeper for a Nuée group: the machine named {name}, id {id}.\n\
         \n\
         It is already on the group's roster. To start it, either:\n\
         - create a Linux server (Ubuntu 22.04 or later, Debian 12 or later) and paste\n\
         \x20 cloud-init.yaml as its user data, or\n\
         - copy install.sh to a server you have and run it as root: sh install.sh\n\
         \n\
         The server needs no open port: it reaches the group through its relay. Check that\n\
         it is on line with `nuee group status <link>`, or on the page of the group's link.\n\
         \n\
         Both files hold the server's key. Keep them as secret as a password and delete them\n\
         once the server runs. To revoke the server: nuee group remove <link> {id}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_survives_printf() {
        let key: [u8; 32] = std::array::from_fn(|i| u8::try_from(i * 8).unwrap_or(0));
        let out = std::process::Command::new("sh")
            .args(["-c", &format!("printf '{}'", octal(&key))])
            .output()
            .expect("sh");
        assert_eq!(out.stdout, key);
    }

    #[test]
    fn the_script_is_valid_shell_and_quotes_the_link() {
        let script = install_script("https://x/#/G/AB'C", &[7; 32], "1.2.3", "server");
        let path = tempfile::NamedTempFile::new().expect("temp");
        std::fs::write(path.path(), &script).expect("write");
        let check = std::process::Command::new("sh")
            .arg("-n")
            .arg(path.path())
            .status()
            .expect("sh -n");
        assert!(check.success());
        assert!(script.contains("printf '%s\\n' 'https://x/#/G/AB'\\''C'"));
        assert!(script.contains("VERSION=1.2.3"));
    }

    #[test]
    fn cloud_init_carries_the_whole_script() {
        let script = install_script("https://x/#/G/A", &[1; 32], "1.2.3", "server");
        let yaml = cloud_init(&script);
        assert!(yaml.starts_with("#cloud-config\n"));
        let back: Vec<&str> = yaml
            .lines()
            .skip_while(|l| !l.ends_with("content: |"))
            .skip(1)
            .take_while(|l| !l.starts_with("runcmd:"))
            .map(|l| l.strip_prefix("      ").unwrap_or(l))
            .collect();
        assert_eq!(back.join("\n").trim_end(), script.trim_end());
    }
}
