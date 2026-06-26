#!/bin/sh
set -eu

: "${PUID:=1000}"
: "${PGID:=1000}"
: "${USER_NAME:=finguard_rs_backend}"
: "${XDG_DATA_HOME:=/data}"
: "${XDG_CONFIG_HOME:=/config}"

log() {
    printf '%s\n' "$*" >&2
}

ensure_group() {
    target_gid="$1"
    name="$2"

    if getent group "${target_gid}" >/dev/null 2>&1; then
        existing_group="$(getent group "${target_gid}" | cut -d: -f1)"
        log "Group with GID ${target_gid} already exists as '${existing_group}'"
        GROUP_NAME="${existing_group}"
        return 0
    fi

    if getent group "${name}" >/dev/null 2>&1; then
        log "Group '${name}' exists but not with GID ${target_gid}; attempting groupmod"
        groupmod -g "${target_gid}" "${name}" 2>/dev/null || true
        GROUP_NAME="${name}"
        return 0
    fi

    log "Creating group '${name}' with GID ${target_gid}"
    groupadd -g "${target_gid}" "${name}" 2>/dev/null || true
    GROUP_NAME="${name}"
}

ensure_user() {
    target_uid="$1"
    target_gid="$2"
    name="$3"

    if getent passwd "${target_uid}" >/dev/null 2>&1; then
        EXISTING_USER="$(getent passwd "${target_uid}" | cut -d: -f1)"
        log "User with UID ${target_uid} already exists as '${EXISTING_USER}', will use that user"
        USER_NAME="${EXISTING_USER}"
        return 0
    fi

    if getent passwd "${name}" >/dev/null 2>&1; then
        log "User '${name}' exists; attempting to ensure UID ${target_uid} and GID ${target_gid}"
        if [ -n "${GROUP_NAME:-}" ]; then
            usermod -g "${GROUP_NAME}" "${name}" 2>/dev/null || true
        else
            usermod -g "${target_gid}" "${name}" 2>/dev/null || true
        fi
        usermod -u "${target_uid}" "${name}" 2>/dev/null || true
        USER_NAME="${name}"
        return 0
    fi

    log "Creating user '${name}' with UID ${target_uid} and GID ${target_gid}"
    useradd -u "${target_uid}" -g "${target_gid}" -m -d "/home/${name}" -s /bin/sh "${name}" 2>/dev/null || true
    USER_NAME="${name}"
}

ensure_dirs_owned() {
    uid="$1"
    gid="$2"

    # dirs="${XDG_DATA_HOME} ${XDG_CONFIG_HOME} /app /home/${USER_NAME}"
    dirs="/data/finguard /config/finguard /app /home/${USER_NAME}"

    for d in ${dirs}; do
        [ -z "${d}" ] && continue
        if [ ! -d "${d}" ]; then
            log "Creating directory ${d}"
            mkdir -p "${d}" || true
        fi

        current_owner="$(stat -c '%u:%g' "${d}" 2>/dev/null || echo '0:0')"
        if [ "${current_owner}" != "${uid}:${gid}" ]; then
            log "chown -R ${uid}:${gid} ${d}"
            chown -R "${uid}:${gid}" "${d}" 2>/dev/null || true
        else
            log "Ownership for ${d} already ${current_owner}"
        fi
    done
    
}


drop_privileges_and_exec() {
    target_user="$1"
    shift

    if [ "$(id -u)" != "0" ]; then
        log "Not running as root (uid=$(id -u)). Executing command as current user."
        exec "$@"
    fi

    if command -v gosu >/dev/null 2>&1; then
        log "Dropping privileges using gosu -> ${target_user}"
        exec gosu "${target_user}" "$@"
    fi

    if command -v su >/dev/null 2>&1; then
        log "Dropping privileges using su -> ${target_user}"
        cmd="$*"
        exec su -s /bin/sh "${target_user}" -c "${cmd}"
    fi

    log "No privilege-dropping helper found; continuing as root (not recommended)"
    exec "$@"
}

main() {
    log "Entrypoint starting: PUID=${PUID} PGID=${PGID} USER_NAME=${USER_NAME}"
    ensure_group "${PGID}" "${USER_NAME}"
    ensure_user  "${PUID}" "${PGID}" "${USER_NAME}"
    ensure_dirs_owned "${PUID}" "${PGID}"

    if [ "$#" -eq 0 ]; then
        set -- finguard-rs --host "0.0.0.0" --port "${FINGUARD_PORT:-3111}"
    fi

    drop_privileges_and_exec "${USER_NAME}" "$@"
}

main "$@"
