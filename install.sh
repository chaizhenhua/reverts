#!/bin/sh
# ReverTS installer — downloads the pre-compiled `reverts-cli` binary and the
# skill bundle from GitHub Releases and installs both locally. No `reverts-mcp`
# server is involved: the skills drive `reverts-cli` directly.
#
#   curl -fsSL https://raw.githubusercontent.com/chaizhenhua/reverts/main/install.sh | sh
#
# Environment overrides:
#   REVERTS_VERSION   release tag to install (default: latest)
#   REVERTS_HOME      install prefix for the binary (default: ~/.reverts)
#   REVERTS_SKILLS_DIR  skill install dir (default: ~/.claude/skills; also
#                       installs into ~/.codex/skills when that dir exists)
#   REVERTS_NO_SKILLS=1 install only the binary, skip skills
#   REVERTS_BASE_URL  override the asset base URL (e.g. a mirror, or a local
#                     file://… dir holding reverts-<target>.tar.gz for testing);
#                     when set, REVERTS_VERSION is ignored.
set -eu

REPO="chaizhenhua/reverts"
VERSION="${REVERTS_VERSION:-latest}"
REVERTS_HOME="${REVERTS_HOME:-${HOME}/.reverts}"
BIN_DIR="${REVERTS_HOME}/bin"

err() { printf 'error: %s\n' "$1" >&2; exit 1; }
info() { printf '%s\n' "$1" >&2; }

need() { command -v "$1" >/dev/null 2>&1 || err "required command not found: $1"; }
need uname
need tar
need mkdir

# Pick a downloader.
if command -v curl >/dev/null 2>&1; then
    dl() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
    dl() { wget -qO "$2" "$1"; }
else
    err "need curl or wget to download release assets"
fi

# Detect platform → release target triple.
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
    Linux)  os_part="unknown-linux-gnu" ;;
    Darwin) os_part="apple-darwin" ;;
    *) err "unsupported OS: $os (supported: Linux, macOS)" ;;
esac
case "$arch" in
    x86_64|amd64)  arch_part="x86_64" ;;
    aarch64|arm64) arch_part="aarch64" ;;
    *) err "unsupported architecture: $arch (supported: x86_64, aarch64)" ;;
esac
target="${arch_part}-${os_part}"

asset="reverts-${target}.tar.gz"
if [ -n "${REVERTS_BASE_URL:-}" ]; then
    base="${REVERTS_BASE_URL%/}"
elif [ "$VERSION" = "latest" ]; then
    base="https://github.com/${REPO}/releases/latest/download"
else
    base="https://github.com/${REPO}/releases/download/${VERSION}"
fi
url="${base}/${asset}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

info "Downloading ${asset} (${VERSION})..."
dl "$url" "${tmp}/${asset}" || err "download failed: $url"

# Verify checksum when the sidecar is published and a hasher is available.
if dl "${url}.sha256" "${tmp}/${asset}.sha256" 2>/dev/null; then
    if command -v sha256sum >/dev/null 2>&1; then
        hasher="sha256sum"
    elif command -v shasum >/dev/null 2>&1; then
        hasher="shasum -a 256"
    else
        hasher=""
    fi
    if [ -n "$hasher" ]; then
        expected="$(awk '{print $1}' "${tmp}/${asset}.sha256")"
        actual="$($hasher "${tmp}/${asset}" | awk '{print $1}')"
        [ "$expected" = "$actual" ] || err "checksum mismatch for ${asset}"
        info "Checksum OK."
    fi
fi

tar -xzf "${tmp}/${asset}" -C "$tmp"
extracted="${tmp}/reverts-${target}"
[ -f "${extracted}/bin/reverts-cli" ] || err "archive missing bin/reverts-cli"

# Install the binary.
mkdir -p "$BIN_DIR"
install -m 0755 "${extracted}/bin/reverts-cli" "${BIN_DIR}/reverts-cli" 2>/dev/null \
    || { cp "${extracted}/bin/reverts-cli" "${BIN_DIR}/reverts-cli"; chmod 0755 "${BIN_DIR}/reverts-cli"; }
info "Installed reverts-cli -> ${BIN_DIR}/reverts-cli"

# Install the skills (unless suppressed).
if [ "${REVERTS_NO_SKILLS:-0}" != "1" ] && [ -d "${extracted}/skills" ]; then
    install_skills() {
        dest="$1"
        mkdir -p "$dest"
        for entry in "${extracted}/skills"/*/; do
            [ -f "${entry}SKILL.md" ] || continue
            name="$(basename "$entry")"
            link="${dest}/${name}"
            if [ -e "$link" ] && [ ! -L "$link" ] && [ ! -e "$link/.reverts-managed" ]; then
                info "skip: ${link} exists and is not managed by this installer"
                continue
            fi
            rm -rf "$link"
            cp -R "${entry%/}" "$link"
            : > "${link}/.reverts-managed"
        done
        info "Installed skills -> ${dest}"
    }
    install_skills "${REVERTS_SKILLS_DIR:-${HOME}/.claude/skills}"
    # Codex uses a parallel loader; install there too when present.
    if [ -z "${REVERTS_SKILLS_DIR:-}" ] && [ -d "${HOME}/.codex/skills" ]; then
        install_skills "${HOME}/.codex/skills"
    fi
fi

# PATH guidance.
case ":${PATH}:" in
    *":${BIN_DIR}:"*) ;;
    *)
        info ""
        info "Add ${BIN_DIR} to your PATH, e.g.:"
        info "  echo 'export PATH=\"${BIN_DIR}:\$PATH\"' >> ~/.profile"
        ;;
esac

info ""
info "Done. Restart Claude/Codex (or run /mcp) so the skill registry rebinds."
info "Verify the binary with: ${BIN_DIR}/reverts-cli --version"
