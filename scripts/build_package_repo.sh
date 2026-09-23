#!/usr/bin/env bash
# Build the signed apt and dnf repositories served from GitHub Pages.
#
# Usage:
#   REPO_URL=https://nfishel48.github.io/Jotter REPO_GPG_KEY_ID=<fingerprint> \
#     scripts/build_package_repo.sh <dir of .deb/.rpm files> <output dir>
#
# Needs apt-ftparchive (apt-utils), createrepo_c, rpmsign (rpm) and gpg with
# the signing key's secret half already imported. release.yml's publish-repos
# job runs it over the packages of the last few releases, so the site holds
# only those: it is rebuilt from scratch on every release rather than grown.
#
# Output layout:
#   jotter.asc      the public signing key (apt and dnf both accept it armored)
#   jotter.sources  apt source, deb822 format -> /etc/apt/sources.list.d/
#   jotter.repo     dnf repository             -> /etc/yum.repos.d/
#   deb/            apt: dists/stable/{InRelease,Release,Release.gpg}, pool/
#   rpm/            dnf: signed packages, repodata/ with repomd.xml.asc
#   index.html      the install instructions

set -euo pipefail

IN="${1:?usage: $0 <packages dir> <output dir>}"
OUT="${2:?usage: $0 <packages dir> <output dir>}"
: "${REPO_URL:?set REPO_URL to the URL the output directory will be served at}"
: "${REPO_GPG_KEY_ID:?set REPO_GPG_KEY_ID to the signing key fingerprint}"
REPO_URL="${REPO_URL%/}"

shopt -s nullglob
debs=("$IN"/*.deb)
rpms=("$IN"/*.rpm)
(( ${#debs[@]} )) || { echo "no .deb files in $IN" >&2; exit 1; }
(( ${#rpms[@]} )) || { echo "no .rpm files in $IN" >&2; exit 1; }

# Emptied rather than removed, so the output may be a mount point.
mkdir -p "$OUT"
find "$OUT" -mindepth 1 -delete
mkdir -p "$OUT/deb/pool/main" "$OUT/rpm"

gpg --batch --armor --export "$REPO_GPG_KEY_ID" > "$OUT/jotter.asc"
[[ -s "$OUT/jotter.asc" ]] || { echo "no public key for $REPO_GPG_KEY_ID" >&2; exit 1; }

echo "==> apt"
cp "${debs[@]}" "$OUT/deb/pool/main/"
(
  cd "$OUT/deb"
  archs="$(for f in pool/main/*.deb; do dpkg-deb -f "$f" Architecture; done | sort -u | tr '\n' ' ')"
  for arch in $archs; do
    dir="dists/stable/main/binary-$arch"
    mkdir -p "$dir"
    apt-ftparchive --arch "$arch" packages pool > "$dir/Packages"
    gzip -9kn "$dir/Packages"
  done
  apt-ftparchive \
    -o APT::FTPArchive::Release::Origin=Jotter \
    -o APT::FTPArchive::Release::Label=Jotter \
    -o APT::FTPArchive::Release::Suite=stable \
    -o APT::FTPArchive::Release::Codename=stable \
    -o APT::FTPArchive::Release::Components=main \
    -o "APT::FTPArchive::Release::Architectures=${archs% }" \
    release dists/stable > Release.tmp
  mv Release.tmp dists/stable/Release
  gpg --batch --yes --local-user "$REPO_GPG_KEY_ID" --clearsign \
    -o dists/stable/InRelease dists/stable/Release
  gpg --batch --yes --local-user "$REPO_GPG_KEY_ID" --armor --detach-sign \
    -o dists/stable/Release.gpg dists/stable/Release
)

echo "==> dnf"
cp "${rpms[@]}" "$OUT/rpm/"
# Packages and metadata are both signed, so dnf can check each (gpgcheck and
# repo_gpgcheck below). This re-signs the repository's copies only; the .rpm
# attached to the GitHub release stays unsigned. `__gpg` because Debian and
# Ubuntu's rpm looks for a `gpg2` that their gnupg does not install.
rpmsign --addsign \
  --define "__gpg $(command -v gpg)" \
  --define "_gpg_name $REPO_GPG_KEY_ID" \
  "$OUT"/rpm/*.rpm
createrepo_c --quiet "$OUT/rpm"
gpg --batch --yes --local-user "$REPO_GPG_KEY_ID" --armor --detach-sign \
  -o "$OUT/rpm/repodata/repomd.xml.asc" "$OUT/rpm/repodata/repomd.xml"

cat > "$OUT/jotter.sources" <<EOF
Types: deb
URIs: $REPO_URL/deb
Suites: stable
Components: main
Signed-By: /usr/share/keyrings/jotter.asc
EOF

cat > "$OUT/jotter.repo" <<EOF
[jotter]
name=Jotter
baseurl=$REPO_URL/rpm
enabled=1
gpgcheck=1
repo_gpgcheck=1
gpgkey=$REPO_URL/jotter.asc
EOF

cat > "$OUT/index.html" <<EOF
<!doctype html>
<meta charset="utf-8">
<title>Jotter packages</title>
<style>body{font:16px/1.5 system-ui,sans-serif;max-width:46rem;margin:2rem auto;padding:0 1rem}pre{background:#f4f4f4;padding:.75rem;overflow-x:auto}</style>
<h1>Jotter packages</h1>
<p>Package repositories for <a href="https://github.com/nfishel48/Jotter">Jotter</a>.
Linux x86_64, Debian 12 / Ubuntu 24.04 / Fedora or newer, with PipeWire.</p>
<h2>Debian, Ubuntu (apt)</h2>
<pre>sudo curl -fsSLo /usr/share/keyrings/jotter.asc $REPO_URL/jotter.asc
sudo curl -fsSLo /etc/apt/sources.list.d/jotter.sources $REPO_URL/jotter.sources
sudo apt update &amp;&amp; sudo apt install jotter</pre>
<h2>Fedora (dnf)</h2>
<pre>sudo curl -fsSLo /etc/yum.repos.d/jotter.repo $REPO_URL/jotter.repo
sudo dnf install jotter</pre>
<p>Then fetch the speech models once: <code>jotter models pull</code>.</p>
EOF

echo "built $OUT"
find "$OUT" -type f | sort
