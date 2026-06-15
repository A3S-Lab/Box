# Supervising the monitor (`a3s-box monitor`)

`a3s-box` is daemonless: a detached box (`run -d`) is supervised by a separate
`a3s-box monitor` process that polls `boxes.json`, restarts dead boxes per their
`--restart` policy, and acts on health checks (with exponential backoff).

On its own the monitor is just a foreground process. **If it isn't running,
detached boxes silently lose restart + health supervision** — nothing restarts a
crashed box, and `status` can stay `starting`. Running it by hand
(`a3s-box monitor &`) works until that shell or process dies.

## Install it as a supervised service (recommended)

```bash
a3s-box monitor --install            # default 5s poll
a3s-box monitor --install --interval 10
```

This installs and enables a **per-user** service — no root required:

| OS | Mechanism | Unit |
|----|-----------|------|
| Linux | systemd `--user` | `~/.config/systemd/user/a3s-box-monitor.service` |
| macOS | launchd LaunchAgent | `~/Library/LaunchAgents/com.a3s-box.monitor.plist` |

The OS keeps the monitor running and restarts it if it crashes
(`Restart=always` / `KeepAlive`). The unit's `ExecStart` points at the absolute
path of the `a3s-box` binary you ran `--install` with.

### Linux: headless hosts

`systemctl --user` services stop when your login session ends. For a server that
should supervise boxes without an active login, enable lingering once:

```bash
loginctl enable-linger "$USER"
```

### Remove it

```bash
a3s-box monitor --uninstall
```

If automatic enable/load fails (e.g. systemd/launchctl unavailable in your
environment), `--install` still writes the unit file and prints the manual
`systemctl --user enable --now …` / `launchctl load -w …` command to finish.

## Metrics + health endpoint

The monitor is the one always-on process and already polls every box's state,
so it can also export operator-scrapable observability. Pass `--metrics-addr`:

```bash
a3s-box monitor --metrics-addr 127.0.0.1:9100
```

It then serves (plain HTTP, **off by default**, bind loopback — there is no auth):

| Path | Response |
|------|----------|
| `GET /healthz` | `200 ok` — liveness probe |
| `GET /metrics` | Prometheus text of box-state metrics |

Exported metrics (read fresh from state per scrape, no side effects):

```
a3s_box_total                         # boxes tracked
a3s_box_state{status="running"|"paused"|"dead"|"created"|"other"}
a3s_box_restarts_total                # sum of per-box restart counts
a3s_box_health{status="healthy"|"unhealthy"}
```

To run the metrics endpoint under the installed service, append the flag to the
unit's `ExecStart`/`ProgramArguments` (or re-run `--install` after we wire a
`--metrics-addr` passthrough). A Prometheus scrape config simply targets
`127.0.0.1:9100`.
