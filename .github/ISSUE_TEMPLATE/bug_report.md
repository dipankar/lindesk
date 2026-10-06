---
name: Bug report
about: Something lindesk did wrong, or didn't do
labels: bug
---

**What happened, and what did you expect?**

**How to reproduce it**

**Environment**

Paste the output of:

```
lindeskctl --version
lindeskctl doctor --json
```

(`doctor --json` contains your kernel, desktop, hardware summary and unit names; remove
anything you'd rather not share.)

**Logs, if relevant**

```
journalctl -u lindeskd -n 100 --no-pager
journalctl --user -u lindesk-session -n 100 --no-pager
```
