#!/usr/bin/env bash
#
# Run the differential suite against the GNU Make that each supported distro
# ships. Make grew functions across that range -- 4.2.1 on RHEL 8 through 4.4.1
# on Debian 13 and Arch -- so a construct that agrees with one make can still
# disagree with another, and tests/differential.rs gates on the version that
# introduced each one.
#
# Only the differential suite runs here, from a statically linked binary built
# once by the caller, so a distro costs a container pull rather than a Rust
# toolchain install. Every image is tried even after one fails, because knowing
# whether a break is one distro or all of them is the useful part.
#
# Usage: .github/scripts/make-compat.sh path/to/differential-test-binary

set -uo pipefail

binary=${1:-}
if [[ -z $binary || ! -x $binary ]]; then
	echo "usage: $0 <differential test binary>" >&2
	exit 2
fi
binary=$(cd "$(dirname "$binary")" && pwd)/$(basename "$binary")

# image<TAB>command that installs make. Kept as one list so adding a distro is
# a one-line change; the versions in the comments are what these images shipped
# on 2026-09-09.
IMAGES=(
	"almalinux:8	dnf install -y make"        # 4.2.1, and RHEL 8's make
	"almalinux:9	dnf install -y make"        # 4.3
	"almalinux:10	dnf install -y make"        # 4.4.1
	"debian:12	apt-get update && apt-get install -y make"  # 4.3, oldstable
	"debian:13	apt-get update && apt-get install -y make"  # 4.4.1, stable
	"ubuntu:22.04	apt-get update && apt-get install -y make"  # 4.3
	"ubuntu:24.04	apt-get update && apt-get install -y make"  # 4.3
	"ubuntu:26.04	apt-get update && apt-get install -y make"  # 4.4.1
	"archlinux:latest	pacman -Sy --noconfirm make"        # 4.4.1, rolling
)

summary=()
failed=0

for entry in "${IMAGES[@]}"; do
	image=${entry%%$'\t'*}
	install=${entry#*$'\t'}

	echo "::group::$image"
	# The binary is static, so nothing but make and a POSIX userland is needed.
	log=$(docker run --rm -v "$binary:/differential:ro" "$image" sh -c "
		{ $install; } >/dev/null 2>&1 || { echo 'could not install make'; exit 90; }
		make --version | head -1
		/differential --test-threads 1
	" 2>&1)
	status=$?
	echo "$log"
	echo "::endgroup::"

	version=$(grep -m1 -o 'GNU Make [0-9.]*' <<<"$log" || echo 'make unknown')
	if ((status == 0)); then
		summary+=("$(printf 'pass  %-20s %s' "$image" "$version")")
	else
		summary+=("$(printf 'FAIL  %-20s %s' "$image" "$version")")
		failed=1
		echo "::error title=make compat::$image ($version) failed the differential suite"
	fi
done

printf '\n%s\n' '--- make compatibility ---'
printf '%s\n' "${summary[@]}"

exit $failed
