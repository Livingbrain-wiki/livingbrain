import Foundation

/// Where an update is staged, and the small shell helper that swaps the
/// installed bundle once the app has quit.
///
/// The helper is never given paths inside its text: the app passes every
/// path as a separate argument, and the script reads them as "$1"…"$6".
/// Nothing runs with elevated privileges; the install location must be
/// writable by the user, which the app checks first.
public enum UpdateHelper {
    /// Positional arguments, in order. The script and the app both use this list.
    public static let argumentOrder = ["pid", "new", "installed", "backup", "workdir", "opener"]

    public struct Paths: Equatable, Sendable {
        /// The verified copy, next to the installed bundle (same volume, so the move is a rename).
        public let staging: URL
        /// Where the old bundle is moved while the new one is put in place.
        public let backup: URL
    }

    /// Staging and backup names sit beside the installed bundle, with a
    /// per-update id so nothing stale can collide with them.
    public static func paths(installed: URL, id: String) -> Paths {
        let parent = installed.deletingLastPathComponent()
        let name = installed.deletingPathExtension().lastPathComponent
        return Paths(
            staging: parent.appendingPathComponent(".\(name).update-\(id).app", isDirectory: true),
            backup: parent.appendingPathComponent(".\(name).previous-\(id).app", isDirectory: true)
        )
    }

    /// The helper text. Waits for the app to exit, validates the new bundle,
    /// swaps it in (restoring the old one if that fails), removes the
    /// quarantine flag from the new bundle only, relaunches, and removes the
    /// working directory on success.
    public static let script = #"""
    #!/bin/sh
    # Living Brain update helper. Written by the app for one update and run
    # as the user, never with privileges.
    # Arguments: app pid, new bundle, installed bundle, backup path, work
    # directory (removed on success), opener (/usr/bin/open).
    pid="$1"
    new="$2"
    installed="$3"
    backup="$4"
    workdir="$5"
    opener="$6"

    log() { printf '%s\n' "$1" >>"$workdir/update.log" 2>/dev/null; }
    relaunch() {
      "$opener" "$installed" >/dev/null 2>&1 || log "could not relaunch $installed"
    }

    for value in "$pid" "$new" "$installed" "$backup" "$workdir" "$opener"; do
      if [ -z "$value" ]; then exit 2; fi
    done
    case "$pid" in
      ''|*[!0-9]*) log "bad pid"; exit 2 ;;
    esac
    if [ "$pid" -le 1 ]; then log "bad pid"; exit 2; fi

    # Wait up to 60 s for the app to quit, so its bundle is no longer in use.
    waited=0
    while kill -0 "$pid" 2>/dev/null; do
      if [ "$waited" -ge 300 ]; then
        log "the app did not quit within 60 s; the update was not installed"
        exit 1
      fi
      sleep 0.2
      waited=$((waited + 1))
    done

    # Validate the staged copy before it replaces anything.
    if [ ! -d "$new" ]; then
      log "the staged update is missing"
      relaunch
      exit 1
    fi
    /usr/bin/xattr -dr com.apple.quarantine "$new" >/dev/null 2>&1
    if ! /usr/bin/codesign --verify --deep --strict "$new" >/dev/null 2>&1; then
      log "the staged update failed its signature check; nothing was replaced"
      rm -rf "$new"
      relaunch
      exit 1
    fi

    # Swap: old bundle aside, new bundle into place. A failed move back is the
    # one case that needs the user, so the backup path is logged.
    rm -rf "$backup"
    if ! mv "$installed" "$backup"; then
      log "could not move the installed app aside; nothing was replaced"
      rm -rf "$new"
      relaunch
      exit 1
    fi
    if ! mv "$new" "$installed"; then
      log "could not move the new app into place; restoring the previous one"
      if ! mv "$backup" "$installed"; then
        log "RESTORE FAILED: the previous app is at $backup"
      fi
      relaunch
      exit 1
    fi

    relaunch
    rm -rf "$backup"
    rm -rf "$workdir"
    exit 0
    """#

    /// Writes the helper into `directory` and returns its URL.
    public static func writeScript(in directory: URL) throws -> URL {
        let url = directory.appendingPathComponent("update-helper.sh")
        try script.write(to: url, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: url.path)
        return url
    }
}
