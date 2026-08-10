# Security policy

Please report security-sensitive issues privately through GitHub's security-advisory interface for
this repository. Do not include credentials, private configuration, full environment dumps, or
unredacted EDID serial numbers in public reports.

Only the current release and the pinned niri revision in `upstream.lock` are supported. This project
controls display hardware directly; test untrusted changes from a separate login session and keep
your distribution niri binary available for rollback.
