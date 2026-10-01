# Your own master

`iw4l-master` is a relay and server browser, not a game server: the host's client
simulates the match. It pulls in no engine dependency, so a VPS needs no assets and no GPU. Publishing our own releases: [`DEPLOY.md`](DEPLOY.md).

## Install — from the machine with the clone; the VPS needs only ssh

```bash
cargo xtask master install root@1.2.3.4
cargo xtask master logs    root@1.2.3.4 --since 10min
```

`install` mints a CA and server certificate under `~/.iw4l/ca`, builds a static
binary, installs it with the certificates under `/usr/local/lib/iw4l/` and
`/etc/iw4l/`, writes the unit through `iw4l-master print-unit` and runs
`enable --now`. The CA key never leaves the local machine. `master update`,
`status` and `uninstall` follow, each taking `--channel dev` and `--ca DIR`;
`uninstall` keeps the certificates and CA, so a reinstall stays trusted.

## No domain required — the certificate is not tied to one

The client connects by address but checks the certificate against a **separate**
name — `IW4L_MASTER_SERVER_NAME`, a fixed label — so nothing in the SAN depends on
the host; with `IW4L_MASTER_CA_CERT` set it loads only that PEM into an empty
`RootCertStore` (`net/src/transport/master.rs:3107`), bypassing the platform
verifier. `install` signs `San::Labels` alone: the certificate is minted **once
per user, not per server**, and a new IP or VPS keeps it.

## What it records — nothing on disk

Rooms, the advertised match name, peers and room membership live in `ServiceState`
in memory, gone when the room closes or the process restarts. No account, history,
analytics or telemetry. What survives is the systemd journal —
startup and failures, not matches or players — under the VPS's own `journald`
retention. Peer IPs are visible to the kernel and to any packet capture on that
host while a connection is open, as with any server. Run it for others and that is
the honest description: it forwards packets and vouches for nobody.

## Hand out to players — this block plus `iw4l-ca.pem`

Give every player these settings and the public CA file over a trusted channel.
Add the settings to their existing `.env`, replacing the address and CA path:

```dotenv
IW4L_MASTER_ADDR=1.2.3.4:4433
IW4L_MASTER_SERVER_NAME=iw4l-prod
IW4L_MASTER_CA_CERT=/path/to/iw4l-ca.pem
```

This example is for the default `prod` installation. With `--channel dev`, use
port `4434` and server name `iw4l-dev` instead.

| Setting | What it changes and when to set it |
| --- | --- |
| `IW4L_MASTER_ADDR` | The master to contact: a hostname or IP **with a port**, such as `1.2.3.4:4433` or `[::1]:4433`. The game has no built-in address; if unset, master networking is disabled. Set it on hosts and joining players. |
| `IW4L_MASTER_SERVER_NAME` | The name to check in the server certificate, independently of its IP. Required whenever the address is set; there is no built-in name. Use `iw4l-prod` for the default installation, not the VPS hostname. |
| `IW4L_MASTER_CA_CERT` | Path to the PEM CA file that lets the client trust your master. Set it for the private CA created by `install`. If unset, the client uses the platform certificate verifier; if set, only certificates from this file are trusted. Relative paths use the process working directory, which on Windows is the folder containing `iw4l.exe`. |
| `IW4L_MASTER_HOST_NAME` | The room name other players see. To host via `map`, set a name that is not empty or only whitespace, and leave `IW4L_MASTER_JOIN` unset. Names can occupy at most 48 UTF-8 bytes. Hosting through the menu uses `iw4l host` if the name is unset. Joining through the menu needs no host name. |
| `IW4L_MASTER_MAX_PLAYERS` | The room capacity, **including the host**. Optional; defaults to `18`. Set an integer from `2` through `18` to limit the room size. It is read only when creating a room, via `map` or the menu; invalid values prevent room creation rather than being clamped. |
| `IW4L_MASTER_PASSWORD` | The room password for command-line hosting and joining. Lobby settings can set, change or remove it; joining a protected room through the menu prompts for it. |

The host can start a match with:

```bash
make map mp_boneyard IW4L_MASTER_HOST_NAME='Friday match' IW4L_MASTER_MAX_PLAYERS=8
```

Other players open `make menu` and select the room. Master networking needs
MW2 multiplayer data (`common_mp.ff`); it is disabled during demo replay.

When a setting appears in more than one place:

* **Direct game launch:** existing environment values win. The game loads one
  `.env`: next to its binary first, otherwise the first found from the working
  directory upwards. It fills only unset variables; files are not merged.
* **GNU make recipes (without `-e`):** command-line assignments as above win
  over the repository `.env`, which wins over inherited environment values
  for keys it defines. The Makefile exports these values to the game.
  See GNU make's [environment rules](https://www.gnu.org/software/make/manual/html_node/Environment.html).
* **Portable Windows launcher:** inherited environment values win over the
  adjacent `.env`; after an explicit update, release-manifest values fill only
  settings still unset. See [`WINDOWS.md`](WINDOWS.md) for the portable layout.
