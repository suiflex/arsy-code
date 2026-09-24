#!/bin/sh
set -eu

repository="suiflex/arsy-code"
version="${ARSY_VERSION:-latest}"

case "$version" in
    *[!A-Za-z0-9._-]*)
        echo "error: invalid ARSY_VERSION: $version" >&2
        exit 1
        ;;
esac

# Releases are tagged v<semver>, so accept a bare version and tag it.
case "$version" in
    [0-9]*) version="v$version" ;;
esac

case "$(uname -s)" in
    Linux) platform="linux" ;;
    Darwin) platform="macos" ;;
    *)
        echo "error: unsupported operating system; use install.ps1 on Windows" >&2
        exit 1
        ;;
esac

case "$(uname -m)" in
    x86_64 | amd64) architecture="x86_64" ;;
    arm64 | aarch64) architecture="aarch64" ;;
    *)
        echo "error: unsupported architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

archive="arsy-${platform}-${architecture}.tar.gz"
if [ -n "${ARSY_DOWNLOAD_BASE:-}" ]; then
    download_base="${ARSY_DOWNLOAD_BASE%/}"
elif [ "$version" = "latest" ]; then
    download_base="https://github.com/${repository}/releases/latest/download"
else
    download_base="https://github.com/${repository}/releases/download/${version}"
fi

temporary_directory="$(mktemp -d)"
trap 'rm -rf "$temporary_directory"' EXIT HUP INT TERM

download() {
    source_url="$1"
    destination="$2"
    case "$source_url" in
        file://*)
            cp "${source_url#file://}" "$destination"
            return
            ;;
    esac
    if command -v curl >/dev/null 2>&1; then
        if [ -t 1 ] && [ -t 2 ]; then
            curl -# -fL --retry 3 --proto '=https' --tlsv1.2 "$source_url" -o "$destination"
        else
            curl -fsSL --retry 3 --proto '=https' --tlsv1.2 "$source_url" -o "$destination"
        fi
    elif command -v wget >/dev/null 2>&1; then
        if [ -t 1 ] && [ -t 2 ]; then
            wget --show-progress -q --https-only "$source_url" -O "$destination"
        else
            wget -q --https-only "$source_url" -O "$destination"
        fi
    else
        echo "error: install curl or wget first" >&2
        exit 1
    fi
}

echo "==> Downloading ARSY CODE (${archive})..."
download "${download_base}/${archive}" "${temporary_directory}/${archive}"
download "${download_base}/${archive}.sha256" "${temporary_directory}/${archive}.sha256"

# Checksum only: this is not the canonical verification path. The canonical
# GitHub Release archive additionally carries a Sigstore signature and
# bundle — see "Install and verify" in docs/34-distribution.md. Verify those
# yourself for anything beyond a quick local install.
echo "==> Verifying SHA-256 checksum..."
expected_checksum="$(tr -d '[:space:]' < "${temporary_directory}/${archive}.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
    actual_checksum="$(sha256sum "${temporary_directory}/${archive}" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
    actual_checksum="$(shasum -a 256 "${temporary_directory}/${archive}" | awk '{print $1}')"
else
    echo "error: SHA-256 tool not found" >&2
    exit 1
fi
if [ "$expected_checksum" != "$actual_checksum" ]; then
    echo "error: checksum verification failed" >&2
    exit 1
fi

echo "==> Extracting binaries..."
tar -xzf "${temporary_directory}/${archive}" -C "$temporary_directory"

: "${HOME:?HOME is not set}"
default_install_directory="${HOME}/.local/bin"
install_directory="${ARSY_INSTALL_DIR:-$default_install_directory}"
mkdir -p "$install_directory"
echo "==> Installing binaries to ${install_directory}..."
install -m 0755 "${temporary_directory}/arsy" "${install_directory}/arsy"
# FluxGuard travels in the same archive and has to land beside `arsy`: that is
# where ARSY looks for it when it declares the bundled MCP server.
install -m 0755 "${temporary_directory}/fluxguard" "${install_directory}/fluxguard"

# The shared ARSY home. Created here so a first run has settings to read and a
# place to keep credentials; an existing file is never touched.
arsy_home="${ARSY_CONFIG_HOME:-${HOME}/.arsy}"
mkdir -p "$arsy_home"
if [ ! -e "${arsy_home}/arsy.json" ]; then
    printf '{}\n' > "${arsy_home}/arsy.json"
fi
PATH="${install_directory}:${PATH}"
export PATH

case ":${PATH}:" in
    *":${install_directory}:"*) ;;
    *)
        echo "error: failed to add ARSY CODE to this process PATH" >&2
        exit 1
        ;;
esac

if [ "$install_directory" = "$default_install_directory" ]; then
    path_line='export PATH="$HOME/.local/bin:$PATH"'
    profile="${ARSY_PROFILE:-}"
    if [ -z "$profile" ]; then
        case "${SHELL:-}" in
            */zsh) profile="${HOME}/.zshrc" ;;
            */bash) profile="${HOME}/.bashrc" ;;
            *) profile="${HOME}/.profile" ;;
        esac
    fi
    if ! grep -F "$path_line" "$profile" >/dev/null 2>&1; then
        printf '\n# ARSY CODE\n%s\n' "$path_line" >> "$profile"
    fi
fi

if [ -t 1 ]; then
    cyan='\033[38;2;53;200;255m'
    reset='\033[0m'
else
    cyan=''
    reset=''
fi

printf '\n'
printf "${cyan}         ++++++==         ${reset}\n"
printf "${cyan}       ***++++++===       ${reset}\n"
printf "${cyan}      ****      +===      ${reset}  ARSY CODE\n"
printf "${cyan}      ***        +==      ${reset}  Auditable, model-independent agent harness\n"
printf "${cyan}   +****   ****   ++===   ${reset}\n"
printf "${cyan}  **+**   +++***   +====  ${reset}\n"
printf "${cyan} ****+   ++++++++   +==== ${reset}\n"
printf "${cyan}*****    ==++++++    +====${reset}\n"
printf '\n'

echo "ARSY CODE installed: ${install_directory}/arsy"
echo "Restart terminal, then run:"
echo "  arsy doctor"
