#!/bin/sh
set -eu

repository_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
temporary_directory="$(mktemp -d)"
trap 'rm -rf "$temporary_directory"' EXIT HUP INT TERM

case "$(uname -s)" in
    Linux) platform="linux" ;;
    Darwin) platform="macos" ;;
    *) exit 0 ;;
esac
case "$(uname -m)" in
    x86_64 | amd64) architecture="x86_64" ;;
    arm64 | aarch64) architecture="aarch64" ;;
    *) exit 0 ;;
esac

release_directory="${temporary_directory}/release"
home_directory="${temporary_directory}/home"
install_directory="${home_directory}/.local/bin"
mkdir -p "$release_directory" "$home_directory"

cat > "${release_directory}/arsy" <<'EOF'
#!/bin/sh
: > "${HOME}/arsy-fake-binary-ran"
EOF
chmod +x "${release_directory}/arsy"

cat > "${release_directory}/fluxguard" <<'EOF'
#!/bin/sh
: > "${HOME}/fluxguard-fake-binary-ran"
EOF
chmod +x "${release_directory}/fluxguard"
cat > "${release_directory}/arsy-sandbox-worker" <<'EOF'
#!/bin/sh
: > "${HOME}/arsy-sandbox-worker-fake-binary-ran"
EOF
chmod +x "${release_directory}/arsy-sandbox-worker"

archive="arsy-${platform}-${architecture}.tar.gz"
tar -C "$release_directory" -czf "${release_directory}/${archive}" arsy fluxguard arsy-sandbox-worker
if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${release_directory}/${archive}" | awk '{print $1}' \
        > "${release_directory}/${archive}.sha256"
else
    shasum -a 256 "${release_directory}/${archive}" | awk '{print $1}' \
        > "${release_directory}/${archive}.sha256"
fi

HOME="$home_directory" \
SHELL="/bin/zsh" \
ARSY_DOWNLOAD_BASE="file://${release_directory}" \
sh "${repository_root}/install.sh" > "${temporary_directory}/output"

test -x "${install_directory}/arsy"
# The backend is discovered beside arsy, not on PATH.
test -x "${install_directory}/arsy-sandbox-worker"
test -x "${install_directory}/fluxguard"
grep -F 'export PATH="$HOME/.local/bin:$PATH"' "${home_directory}/.zshrc" >/dev/null
grep -F "ARSY CODE installed:" "${temporary_directory}/output" >/dev/null

printf '%064d\n' 0 > "${release_directory}/${archive}.sha256"
if HOME="$home_directory" \
    SHELL="/bin/zsh" \
    ARSY_DOWNLOAD_BASE="file://${release_directory}" \
    sh "${repository_root}/install.sh" >/dev/null 2>&1; then
    echo "error: installer accepted an invalid checksum" >&2
    exit 1
fi
rm "${release_directory}/arsy-sandbox-worker"
tar -C "$release_directory" -czf "${release_directory}/${archive}" arsy fluxguard
if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${release_directory}/${archive}" | awk '{print $1}' \
        > "${release_directory}/${archive}.sha256"
else
    shasum -a 256 "${release_directory}/${archive}" | awk '{print $1}' \
        > "${release_directory}/${archive}.sha256"
fi
missing_home="${temporary_directory}/missing-home"
missing_install="${temporary_directory}/missing-bin"
mkdir -p "$missing_home"
if HOME="$missing_home" \
    ARSY_INSTALL_DIR="$missing_install" \
    ARSY_DOWNLOAD_BASE="file://${release_directory}" \
    sh "${repository_root}/install.sh" >"${temporary_directory}/missing-output" 2>&1; then
    echo "error: installer accepted an archive without arsy-sandbox-worker" >&2
    exit 1
fi
test ! -e "${missing_install}/arsy"
test ! -e "${missing_home}/.arsy/arsy.json"
grep -F 'Release archive is missing arsy-sandbox-worker' "${temporary_directory}/missing-output" >/dev/null

echo "install.sh: ok"
