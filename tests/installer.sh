#!/bin/sh
set -eu
binary=$1
fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT
version=$("$binary" --version)
version=${version#iztop }
asset=iztop-$version-x86_64-linux
mkdir "$fixture/bin" "$fixture/release" "$fixture/dest"
cp "$binary" "$fixture/bin/iztop"
cp "$binary" "$fixture/release/$asset"
(cd "$fixture/release" && sha256sum "$asset" > SHA256SUMS)
export fixture PATH="$fixture/bin:$PATH"
cat > "$fixture/bin/curl" <<'MOCK'
#!/bin/sh
[ "$2" = "-o" ] || exit 1
cp "$fixture/release/${4##*/}" "$3"
MOCK
chmod +x "$fixture/bin/curl"
run_install() {
    sh install.sh --method bin --version "$version" --bindir "$fixture/dest"
}
run_install
cmp "$binary" "$fixture/dest/iztop"
rm "$fixture/release/$asset"
run_install
