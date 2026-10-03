//! Fork: env-selected master target (`IW4L_MASTER_ADDR`), used by
//! `cargo xtask dedicated` and local test rigs. Upstream resolves the master
//! only from the community file chosen by `updater::startup`; with no such file
//! this supplies the target from the environment instead.

use std::path::Path;

use super::{MasterTarget, Result};

pub(super) fn from_env() -> Result<Option<MasterTarget>> {
    from_getter(
        |key| std::env::var(key).ok(),
        |path| std::fs::read_to_string(path).map_err(|e| e.to_string()),
    )
}

fn from_getter(
    get: impl Fn(&str) -> Option<String>,
    read: impl Fn(&Path) -> std::result::Result<String, String>,
) -> Result<Option<MasterTarget>> {
    let Some(address) = get("IW4L_MASTER_ADDR") else {
        return Ok(None);
    };
    let server_name = get("IW4L_MASTER_SERVER_NAME")
        .ok_or("IW4L_MASTER_ADDR requires IW4L_MASTER_SERVER_NAME")?;
    let ca_pem = match get("IW4L_MASTER_CA_CERT") {
        Some(path) => read(Path::new(&path))
            .map_err(|e| format!("IW4L_MASTER_CA_CERT {path}: {e}"))?,
        None => String::new(),
    };
    Ok(Some(MasterTarget {
        address,
        server_name,
        ca_pem,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    fn read_ok(_: &Path) -> std::result::Result<String, String> {
        Ok("PEM".into())
    }

    #[test]
    fn unset_addr_is_none() {
        assert!(from_getter(env(&[]), read_ok).unwrap().is_none());
    }

    #[test]
    fn builds_target_with_ca_text() {
        let t = from_getter(
            env(&[
                ("IW4L_MASTER_ADDR", "127.0.0.1:4433"),
                ("IW4L_MASTER_SERVER_NAME", "m.example"),
                ("IW4L_MASTER_CA_CERT", "ca.pem"),
            ]),
            read_ok,
        )
        .unwrap()
        .unwrap();
        assert_eq!(t.address, "127.0.0.1:4433");
        assert_eq!(t.server_name, "m.example");
        assert_eq!(t.ca_pem, "PEM");
    }

    #[test]
    fn server_name_required() {
        let e = from_getter(env(&[("IW4L_MASTER_ADDR", "a:1")]), read_ok).unwrap_err();
        assert!(e.to_string().contains("SERVER_NAME"));
    }

    #[test]
    fn unreadable_ca_is_error() {
        let e = from_getter(
            env(&[
                ("IW4L_MASTER_ADDR", "a:1"),
                ("IW4L_MASTER_SERVER_NAME", "n"),
                ("IW4L_MASTER_CA_CERT", "missing.pem"),
            ]),
            |_| Err("nope".into()),
        )
        .unwrap_err();
        assert!(e.to_string().contains("missing.pem"));
    }
}
