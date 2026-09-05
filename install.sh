#!/bin/sh
# Install an A3S Box release for Linux or macOS.
set -eu

umask 022

REPOSITORY="A3S-Lab/Box"
API_ROOT="https://api.github.com/repos/$REPOSITORY"
RELEASE_ROOT="https://github.com/$REPOSITORY/releases/download"
MARKER_NAME=".a3s-box-install.json"
PATH_BLOCK_BEGIN="# >>> a3s-box installer >>>"
PATH_BLOCK_END="# <<< a3s-box installer <<<"

requested_version=${A3S_BOX_VERSION:-}
install_dir=${A3S_BOX_INSTALL_DIR:-}
archive_path=${A3S_BOX_ARCHIVE:-}
expected_sha256=${A3S_BOX_SHA256:-}
profile_path=${A3S_BOX_PROFILE:-}
modify_path=1
force=0

temporary_dir=
stage_dir=
backup_container=
backup_path=
rollback_needed=0
previous_moved=0
new_activated=0

say() {
    printf 'a3s-box installer: %s\n' "$*"
}

warn() {
    printf 'a3s-box installer: warning: %s\n' "$*" >&2
}

fail() {
    printf 'a3s-box installer: error: %s\n' "$*" >&2
    exit 1
}

case "${A3S_BOX_NO_MODIFY_PATH:-}" in
    1|true|TRUE|yes|YES) modify_path=0 ;;
    ''|0|false|FALSE|no|NO) ;;
    *) fail "A3S_BOX_NO_MODIFY_PATH must be true or false" ;;
esac

usage() {
    cat <<'EOF'
Install A3S Box on Linux or macOS.

Usage:
  install.sh [options]

Options:
  --version VERSION       Install a release such as v3.2.4 (default: latest).
  --install-dir PATH      Install the self-contained distribution at PATH.
  --archive PATH          Install a local release tarball without downloading.
  --sha256 HEX            Expected SHA-256 for --archive.
  --profile PATH          Shell profile to update.
  --no-modify-path        Do not add the installation directory to PATH.
  --force                 Replace a non-empty directory not owned by this installer.
  -h, --help              Show this help.

Environment equivalents:
  A3S_BOX_VERSION, A3S_BOX_INSTALL_DIR, A3S_BOX_ARCHIVE,
  A3S_BOX_SHA256, A3S_BOX_PROFILE, A3S_BOX_NO_MODIFY_PATH, GITHUB_TOKEN

Local archives require both --version and --sha256.
EOF
}

need_value() {
    option=$1
    value=${2-}
    [ -n "$value" ] || fail "$option requires a value"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            need_value "$1" "${2-}"
            requested_version=$2
            shift 2
            ;;
        --version=*)
            requested_version=${1#*=}
            need_value "--version" "$requested_version"
            shift
            ;;
        --install-dir)
            need_value "$1" "${2-}"
            install_dir=$2
            shift 2
            ;;
        --install-dir=*)
            install_dir=${1#*=}
            need_value "--install-dir" "$install_dir"
            shift
            ;;
        --archive)
            need_value "$1" "${2-}"
            archive_path=$2
            shift 2
            ;;
        --archive=*)
            archive_path=${1#*=}
            need_value "--archive" "$archive_path"
            shift
            ;;
        --sha256)
            need_value "$1" "${2-}"
            expected_sha256=$2
            shift 2
            ;;
        --sha256=*)
            expected_sha256=${1#*=}
            need_value "--sha256" "$expected_sha256"
            shift
            ;;
        --profile)
            need_value "$1" "${2-}"
            profile_path=$2
            shift 2
            ;;
        --profile=*)
            profile_path=${1#*=}
            need_value "--profile" "$profile_path"
            shift
            ;;
        --no-modify-path)
            modify_path=0
            shift
            ;;
        --force)
            force=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --)
            shift
            [ "$#" -eq 0 ] || fail "unexpected positional argument: $1"
            ;;
        -*)
            fail "unknown option: $1"
            ;;
        *)
            fail "unexpected positional argument: $1"
            ;;
    esac
done

cleanup() {
    status=$?
    preserve_backup=0
    trap - EXIT HUP INT TERM

    if [ "$status" -ne 0 ] && [ "$rollback_needed" -eq 1 ]; then
        if [ "$new_activated" -eq 1 ] &&
           [ -n "$install_dir" ] &&
           { [ -e "$install_dir" ] || [ -L "$install_dir" ]; }; then
            rm -rf "$install_dir"
        fi
        if [ "$previous_moved" -eq 1 ]; then
            if [ -n "$backup_path" ] &&
               { [ -e "$backup_path" ] || [ -L "$backup_path" ]; }; then
                if ! mv "$backup_path" "$install_dir"; then
                    preserve_backup=1
                    warn "could not restore the previous installation; it remains at $backup_path"
                fi
            else
                preserve_backup=1
                warn "the previous installation backup is unavailable at $backup_path"
            fi
        fi
    fi

    if [ -n "$stage_dir" ] && [ -d "$stage_dir" ]; then
        rm -rf "$stage_dir"
    fi
    if [ -n "$backup_container" ] &&
       [ -d "$backup_container" ] &&
       [ "$preserve_backup" -eq 0 ]; then
        rm -rf "$backup_container"
    fi
    if [ -n "$temporary_dir" ] && [ -d "$temporary_dir" ]; then
        rm -rf "$temporary_dir"
    fi

    exit "$status"
}

trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

detect_platform() {
    kernel=$(uname -s 2>/dev/null || true)
    machine=$(uname -m 2>/dev/null || true)

    case "$kernel" in
        Linux)
            case "$machine" in
                x86_64|amd64)
                    platform=linux-x86_64
                    ;;
                aarch64|arm64)
                    platform=linux-arm64
                    ;;
                *)
                    fail "Linux architecture '$machine' is unsupported; use x86_64 or arm64"
                    ;;
            esac
            ;;
        Darwin)
            case "$machine" in
                arm64|aarch64)
                    platform=macos-arm64
                    ;;
                x86_64)
                    translated=$(
                        sysctl -in sysctl.proc_translated 2>/dev/null || printf '0'
                    )
                    if [ "$translated" = "1" ]; then
                        platform=macos-arm64
                    else
                        fail "Intel macOS is unsupported; A3S Box requires Apple Silicon"
                    fi
                    ;;
                *)
                    fail "macOS architecture '$machine' is unsupported; A3S Box requires Apple Silicon"
                    ;;
            esac
            ;;
        *)
            fail "operating system '$kernel' is unsupported; use Linux or macOS"
            ;;
    esac
}

normalize_tag() {
    candidate=$1
    case "$candidate" in
        v*) tag=$candidate ;;
        *) tag=v$candidate ;;
    esac

    version_number=${tag#v}
    case "$version_number" in
        ''|*[!0-9A-Za-z.+-]*)
            fail "invalid version '$candidate'"
            ;;
    esac
    case "$version_number" in
        *.*.*) ;;
        *) fail "version '$candidate' must contain major, minor, and patch components" ;;
    esac
}

validate_sha256() {
    digest=$1
    [ "${#digest}" -eq 64 ] || fail "SHA-256 must contain exactly 64 hexadecimal characters"
    case "$digest" in
        *[!0-9A-Fa-f]*) fail "SHA-256 contains a non-hexadecimal character" ;;
    esac
}

download_to() {
    source_url=$1
    destination=$2
    authenticated=${3:-0}

    if [ "$authenticated" -eq 1 ] && [ -n "${GITHUB_TOKEN:-}" ]; then
        case "$GITHUB_TOKEN" in
            *'
'*|*'"'*) fail "GITHUB_TOKEN contains an unsupported character" ;;
        esac
        auth_config=$temporary_dir/curl-auth.conf
        if [ ! -f "$auth_config" ]; then
            (
                umask 077
                printf 'header = "Authorization: Bearer %s"\n' "$GITHUB_TOKEN" \
                    > "$auth_config"
            )
        fi
        curl --fail --silent --show-error --location --retry 3 \
            --proto '=https' --tlsv1.2 \
            --config "$auth_config" \
            -H "Accept: application/vnd.github+json" \
            -H "X-GitHub-Api-Version: 2022-11-28" \
            -o "$destination" "$source_url"
    else
        curl --fail --silent --show-error --location --retry 3 \
            --proto '=https' --tlsv1.2 \
            -H "Accept: application/vnd.github+json" \
            -H "X-GitHub-Api-Version: 2022-11-28" \
            -o "$destination" "$source_url"
    fi
}

release_digest() {
    metadata=$1
    wanted_asset=$2

    awk -v wanted="$wanted_asset" '
        index($0, "\"name\": \"" wanted "\"") {
            in_asset = 1
            next
        }
        in_asset && /"digest":[[:space:]]*/ {
            line = $0
            sub(/^.*"digest":[[:space:]]*"/, "", line)
            sub(/".*$/, "", line)
            print line
            exit
        }
    ' "$metadata"
}

sidecar_digest() {
    manifest=$1
    wanted_asset=$2

    awk -v wanted="$wanted_asset" '
        NF >= 2 {
            name = $2
            sub(/^\*/, "", name)
            if (name == wanted) {
                count++
                digest = $1
            }
        }
        END {
            if (count == 1) {
                print digest
            }
        }
    ' "$manifest"
}

file_sha256() {
    file=$1
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file" | awk '{ print $1 }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$file" | awk '{ print $1 }'
    else
        fail "sha256sum or shasum is required"
    fi
}

normalize_absolute_path() {
    printf '%s\n' "$1" | awk -F/ '
        {
            depth = 0
            for (position = 1; position <= NF; position++) {
                component = $position
                if (component == "" || component == ".") {
                    continue
                }
                if (component == "..") {
                    if (depth > 0) {
                        depth--
                    }
                    continue
                }
                parts[++depth] = component
            }
            if (depth == 0) {
                print "/"
                next
            }
            result = ""
            for (position = 1; position <= depth; position++) {
                result = result "/" parts[position]
            }
            print result
        }
    '
}

assert_safe_install_destination() {
    case "$install_dir" in
        /|/Applications|/Library|/System|/Users|/Volumes|/bin|/boot|/dev|\
        /etc|/home|/lib|/lib64|/media|/mnt|/opt|/private|/proc|/root|/run|\
        /sbin|/srv|/sys|/tmp|/usr|/usr/local|/var|/var/tmp)
            fail "refusing to replace protected system directory $install_dir"
            ;;
    esac

    if [ -n "${HOME:-}" ]; then
        home_directory=$HOME
        case "$home_directory" in
            /*) home_directory=$(normalize_absolute_path "$home_directory") ;;
        esac
        [ "$install_dir" != "$home_directory" ] ||
            fail "refusing to replace the user home directory"
    fi
    if [ -n "${XDG_DATA_HOME:-}" ]; then
        data_directory=$XDG_DATA_HOME
        case "$data_directory" in
            /*) data_directory=$(normalize_absolute_path "$data_directory") ;;
            *) data_directory=$(normalize_absolute_path "$(pwd -P)/$data_directory") ;;
        esac
        [ "$install_dir" != "$data_directory" ] ||
            fail "refusing to replace XDG_DATA_HOME"
    fi
}

validate_version_output() {
    executable=$1
    if ! output=$("$executable" --version 2>&1); then
        fail "installed a3s-box failed its version check: $output"
    fi
    actual_version=$(
        printf '%s\n' "$output" |
            sed -n 's/^a3s-box[[:space:]][[:space:]]*\([^[:space:]][^[:space:]]*\).*$/\1/p' |
            sed -n '1p'
    )
    [ -n "$actual_version" ] ||
        fail "unexpected a3s-box --version output: $output"
    [ "$actual_version" = "$version_number" ] ||
        fail "archive reports a3s-box $actual_version, expected $version_number"
}

select_profile() {
    if [ -n "$profile_path" ]; then
        return
    fi
    [ -n "${HOME:-}" ] || return

    shell_value=${SHELL:-}
    shell_name=${shell_value##*/}
    case "$shell_name" in
        zsh)
            if [ -n "${ZDOTDIR:-}" ]; then
                profile_path=$ZDOTDIR/.zshrc
            else
                profile_path=$HOME/.zshrc
            fi
            ;;
        bash)
            if [ "$(uname -s)" = "Darwin" ]; then
                profile_path=$HOME/.bash_profile
            else
                profile_path=$HOME/.bashrc
            fi
            ;;
        fish)
            if [ -n "${XDG_CONFIG_HOME:-}" ]; then
                profile_path=$XDG_CONFIG_HOME/fish/config.fish
            else
                profile_path=$HOME/.config/fish/config.fish
            fi
            ;;
        *)
            profile_path=$HOME/.profile
            ;;
    esac
}

shell_single_quote() {
    printf '%s' "$1" | sed "s/'/'\\\\''/g"
}

update_profile() {
    select_profile
    if [ -z "$profile_path" ]; then
        warn "HOME is unavailable; add $install_dir to PATH manually"
        return 0
    fi
    case "$profile_path" in
        *'
'*) fail "profile path must not contain a newline" ;;
    esac

    profile_parent=$(dirname "$profile_path")
    mkdir -p "$profile_parent" ||
        fail "cannot create profile directory $profile_parent"

    profile_temporary=$(mktemp "${TMPDIR:-/tmp}/a3s-box-profile.XXXXXX") ||
        fail "cannot create a temporary profile"
    if [ -f "$profile_path" ]; then
        awk -v begin="$PATH_BLOCK_BEGIN" -v end="$PATH_BLOCK_END" '
            $0 == begin { skipping = 1; next }
            $0 == end { skipping = 0; next }
            !skipping { print }
        ' "$profile_path" > "$profile_temporary"
    else
        : > "$profile_temporary"
    fi

    quoted_install_dir=$(shell_single_quote "$install_dir")
    active_shell=${SHELL:-}
    active_shell=${active_shell##*/}
    {
        printf '\n%s\n' "$PATH_BLOCK_BEGIN"
        if [ "$active_shell" = "fish" ]; then
            printf "fish_add_path --global --move '%s'\n" "$quoted_install_dir"
        else
            printf "export PATH='%s':\"\$PATH\"\n" "$quoted_install_dir"
        fi
        printf '%s\n' "$PATH_BLOCK_END"
    } >> "$profile_temporary"

    if [ -L "$profile_path" ]; then
        # Keep dotfile-manager symlinks intact.
        cat "$profile_temporary" > "$profile_path"
        rm -f "$profile_temporary"
    else
        mv "$profile_temporary" "$profile_path"
    fi
    say "updated PATH in $profile_path"
}

detect_platform

if [ -z "$install_dir" ]; then
    if [ -n "${XDG_DATA_HOME:-}" ]; then
        install_dir=$XDG_DATA_HOME/a3s-box
    else
        [ -n "${HOME:-}" ] ||
            fail "HOME is unavailable; specify --install-dir"
        install_dir=$HOME/.local/share/a3s-box
    fi
fi

case "$install_dir" in
    *'
'*) fail "installation path must not contain a newline" ;;
    /*) ;;
    *) install_dir=$(pwd -P)/$install_dir ;;
esac
install_dir=$(normalize_absolute_path "$install_dir")

assert_safe_install_destination
install_parent=$(dirname "$install_dir")
install_name=$(basename "$install_dir")
case "$install_name" in
    ''|.|..) fail "invalid installation path: $install_dir" ;;
esac

if [ -n "$archive_path" ]; then
    [ -n "$requested_version" ] ||
        fail "--archive requires --version (or A3S_BOX_VERSION)"
    [ -n "$expected_sha256" ] ||
        fail "--archive requires --sha256 (or A3S_BOX_SHA256)"
    normalize_tag "$requested_version"
else
    command -v curl >/dev/null 2>&1 ||
        fail "curl is required for release downloads"
fi

temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/a3s-box-install.XXXXXX") ||
    fail "cannot create a temporary directory"

if [ -z "$archive_path" ]; then
    metadata=$temporary_dir/release.json
    if [ -n "$requested_version" ]; then
        normalize_tag "$requested_version"
        api_url=$API_ROOT/releases/tags/$tag
    else
        api_url=$API_ROOT/releases/latest
    fi

    say "resolving ${requested_version:-the latest stable release}"
    download_to "$api_url" "$metadata" 1 ||
        fail "could not resolve the requested GitHub release"

    resolved_tag=$(
        sed -n 's/^[[:space:]]*"tag_name":[[:space:]]*"\([^"]*\)".*$/\1/p' \
            "$metadata" | sed -n '1p'
    )
    [ -n "$resolved_tag" ] || fail "GitHub release metadata has no tag_name"
    if [ -n "$requested_version" ] && [ "$resolved_tag" != "$tag" ]; then
        fail "GitHub returned release $resolved_tag when $tag was requested"
    fi
    normalize_tag "$resolved_tag"

    asset_name=a3s-box-$tag-$platform.tar.gz
    if ! grep -F "\"name\": \"$asset_name\"" "$metadata" >/dev/null 2>&1; then
        fail "release $tag has no asset for $platform"
    fi

    api_digest=$(release_digest "$metadata" "$asset_name")
    case "$api_digest" in
        sha256:*) expected_sha256=${api_digest#sha256:} ;;
        ''|null)
            checksum_url=$RELEASE_ROOT/$tag/$asset_name.sha256
            checksum_file=$temporary_dir/$asset_name.sha256
            if download_to "$checksum_url" "$checksum_file" 0; then
                expected_sha256=$(sidecar_digest "$checksum_file" "$asset_name")
            fi
            [ -n "$expected_sha256" ] ||
                fail "release $tag does not publish a SHA-256 digest for $asset_name"
            ;;
        *)
            fail "release $tag publishes an unsupported digest: $api_digest"
            ;;
    esac

    archive_path=$temporary_dir/$asset_name
    say "downloading $asset_name"
    download_to "$RELEASE_ROOT/$tag/$asset_name" "$archive_path" 0 ||
        fail "could not download $asset_name"
else
    [ -f "$archive_path" ] || fail "archive does not exist: $archive_path"
    archive_parent=$(cd "$(dirname "$archive_path")" && pwd -P) ||
        fail "cannot resolve archive directory"
    archive_path=$archive_parent/$(basename "$archive_path")
    asset_name=a3s-box-$tag-$platform.tar.gz
fi

expected_sha256=$(printf '%s' "$expected_sha256" | tr '[:upper:]' '[:lower:]')
validate_sha256 "$expected_sha256"
actual_sha256=$(file_sha256 "$archive_path" | tr '[:upper:]' '[:lower:]')
[ "$actual_sha256" = "$expected_sha256" ] ||
    fail "SHA-256 mismatch for $(basename "$archive_path"): expected $expected_sha256, found $actual_sha256"
say "verified SHA-256 $actual_sha256"

package_dir=a3s-box-$tag-$platform
archive_listing=$temporary_dir/archive.list
if ! tar -tzf "$archive_path" > "$archive_listing"; then
    fail "archive is not a readable gzip-compressed tar file"
fi
[ -s "$archive_listing" ] || fail "archive is empty"

while IFS= read -r entry; do
    case "$entry" in
        "$package_dir"|"$package_dir/"|"$package_dir/"*) ;;
        *) fail "archive contains an unexpected path: $entry" ;;
    esac
    case "/$entry/" in
        */../*|*/./*) fail "archive contains an unsafe path: $entry" ;;
    esac
done < "$archive_listing"

extract_root=$temporary_dir/extracted
mkdir "$extract_root"
if ! tar -xzf "$archive_path" -C "$extract_root"; then
    fail "could not extract the release archive"
fi
package_root=$extract_root/$package_dir
[ -d "$package_root" ] || fail "archive is missing its $package_dir root"

for required in a3s-box a3s-box-shim a3s-box-guest-init; do
    [ -f "$package_root/$required" ] ||
        fail "archive is missing required file: $required"
    [ -x "$package_root/$required" ] ||
        fail "archive file is not executable: $required"
done
if [ "$platform" = "linux-x86_64" ] || [ "$platform" = "linux-arm64" ]; then
    for required in a3s-oci a3s-oci-agent OCI-RUNTIME-REVISION; do
        [ -f "$package_root/$required" ] ||
            fail "archive is missing required Linux file: $required"
    done
    [ -x "$package_root/a3s-oci" ] ||
        fail "archive file is not executable: a3s-oci"
    [ -x "$package_root/a3s-oci-agent" ] ||
        fail "archive file is not executable: a3s-oci-agent"
fi
validate_version_output "$package_root/a3s-box"

if [ -L "$install_dir" ]; then
    fail "installation path must not be a symbolic link: $install_dir"
fi
if [ -e "$install_dir" ]; then
    [ -d "$install_dir" ] ||
        fail "installation path exists and is not a directory: $install_dir"
    if [ ! -f "$install_dir/$MARKER_NAME" ] &&
       [ -n "$(ls -A "$install_dir" 2>/dev/null)" ] &&
       [ "$force" -ne 1 ]; then
        fail "$install_dir is not managed by this installer; pass --force to replace it"
    fi
fi

mkdir -p "$install_parent" ||
    fail "cannot create installation parent $install_parent"
stage_dir=$(mktemp -d "$install_parent/.a3s-box-stage.XXXXXX") ||
    fail "cannot stage files beside $install_dir"
cp -Rp "$package_root/." "$stage_dir/" ||
    fail "could not copy the release into the installation staging directory"

cat > "$stage_dir/$MARKER_NAME" <<EOF
{
  "schema": 1,
  "version": "$tag",
  "platform": "$platform",
  "sha256": "$actual_sha256"
}
EOF
validate_version_output "$stage_dir/a3s-box"

backup_container=$(mktemp -d "$install_parent/.a3s-box-backup.XXXXXX") ||
    fail "cannot prepare an installation backup"
backup_path=$backup_container/previous
rollback_needed=1
if [ -e "$install_dir" ]; then
    mv "$install_dir" "$backup_path" ||
        fail "could not move the previous installation out of the way"
    previous_moved=1
fi
mv "$stage_dir" "$install_dir" ||
    fail "could not activate the staged installation"
stage_dir=
new_activated=1
validate_version_output "$install_dir/a3s-box"
rollback_needed=0
rm -rf "$backup_container"
backup_container=
backup_path=

if [ "$modify_path" -eq 1 ]; then
    update_profile
fi

say "installed A3S Box $version_number for $platform at $install_dir"
if [ "$modify_path" -eq 1 ]; then
    say "open a new terminal, or run: export PATH='$install_dir':\"\$PATH\""
else
    say "add $install_dir to PATH before invoking a3s-box"
fi
say "verify with: a3s-box --version"
